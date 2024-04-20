/// Validate subgraph schemas by parsing them into `InputSchema` and making
/// sure that they are valid
///
/// The input files must be in a particular format; that can be generated by
/// running this script against graph-node shard(s). Before running it,
/// change the `dbs` variable to list all databases against which it should
/// run.
///
/// ```
/// #! /bin/bash
///
/// read -r -d '' query <<EOF
/// \copy (select to_jsonb(a.*) from (select id, schema from subgraphs.subgraph_manifest) a) to '%s'
/// EOF
///
/// dbs="shard1 shard2 .."
///
/// dir=/var/tmp/schemas
/// mkdir -p $dir
///
/// for db in $dbs
/// do
///     echo "Dump $db"
///     q=$(printf "$query" "$dir/$db.json")
///     psql -qXt service=$db -c "$q"
///     sed -r -i -e 's/\\\\/\\/g' "$dir/$db.json"
/// done
///
/// ```
use clap::Parser;

use graph::data::graphql::ext::DirectiveFinder;
use graph::data::graphql::DirectiveExt;
use graph::data::graphql::DocumentExt;
use graph::data::subgraph::SPEC_VERSION_1_1_0;
use graph::prelude::s;
use graph::prelude::DeploymentHash;
use graph::schema::InputSchema;
use graphql_parser::parse_schema;
use serde::Deserialize;
use std::alloc::GlobalAlloc;
use std::alloc::Layout;
use std::alloc::System;
use std::env;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::process::exit;
use std::str::FromStr;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::{Duration, Instant};

use graph::anyhow::{anyhow, bail, Result};

// Install an allocator that tracks allocation sizes

static ALLOCATED: AtomicUsize = AtomicUsize::new(0);

struct Counter;

unsafe impl GlobalAlloc for Counter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ret = System.alloc(layout);
        if !ret.is_null() {
            ALLOCATED.fetch_add(layout.size(), SeqCst);
        }
        ret
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        ALLOCATED.fetch_sub(layout.size(), SeqCst);
    }
}

#[global_allocator]
static A: Counter = Counter;

pub fn usage(msg: &str) -> ! {
    println!("{}", msg);
    println!("usage: validate schema.graphql ...");
    println!("\nValidate subgraph schemas");
    std::process::exit(1);
}

pub fn ensure<T, E: std::fmt::Display>(res: Result<T, E>, msg: &str) -> T {
    match res {
        Ok(ok) => ok,
        Err(err) => {
            eprintln!("{}:\n    {}", msg, err);
            exit(1)
        }
    }
}

fn subgraph_id(schema: &s::Document) -> DeploymentHash {
    let id = schema
        .get_object_type_definitions()
        .first()
        .and_then(|obj_type| obj_type.find_directive("subgraphId"))
        .and_then(|dir| dir.argument("id"))
        .and_then(|arg| match arg {
            s::Value::String(s) => Some(s.to_owned()),
            _ => None,
        })
        .unwrap_or("unknown".to_string());
    DeploymentHash::new(id).expect("subgraph id is not a valid deployment hash")
}

#[derive(Deserialize)]
struct Entry {
    id: i32,
    schema: String,
}

enum RunMode {
    Validate,
    Size,
}

impl FromStr for RunMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "validate" => Ok(RunMode::Validate),
            "size" => Ok(RunMode::Size),
            _ => Err("Invalid mode".to_string()),
        }
    }
}

#[derive(Parser)]
#[clap(
    name = "validate",
    version = env!("CARGO_PKG_VERSION"),
    author = env!("CARGO_PKG_AUTHORS"),
    about = "Validate subgraph schemas"
)]
struct Opts {
    /// Validate a batch of schemas in bulk. When this is set, the input
    /// files must be JSONL files where each line has an `id` and a `schema`
    #[clap(short, long)]
    batch: bool,
    #[clap(long)]
    api: bool,
    #[clap(short, long, default_value = "validate", possible_values = &["validate", "size"])]
    mode: RunMode,
    /// Subgraph schemas to validate
    #[clap(required = true)]
    schemas: Vec<String>,
}

fn parse(raw: &str, name: &str, api: bool) -> Result<DeploymentHash> {
    let schema = parse_schema(raw)
        .map(|v| v.into_static())
        .map_err(|e| anyhow!("Failed to parse schema sgd{name}: {e}"))?;
    let id = subgraph_id(&schema);
    let input_schema = match InputSchema::parse(&SPEC_VERSION_1_1_0, raw, id.clone()) {
        Ok(schema) => schema,
        Err(e) => {
            bail!("InputSchema: {}[{}]: {}", name, id, e);
        }
    };
    if api {
        let _api_schema = match input_schema.api_schema() {
            Ok(schema) => schema,
            Err(e) => {
                bail!("ApiSchema: {}[{}]: {}", name, id, e);
            }
        };
    }
    Ok(id)
}

trait Runner {
    fn run(&self, raw: &str, name: &str, api: bool);
}

struct Validator;

impl Runner for Validator {
    fn run(&self, raw: &str, name: &str, api: bool) {
        match parse(raw, name, api) {
            Ok(id) => {
                println!("Schema {}[{}]: OK", name, id);
            }
            Err(e) => {
                println!("Error: {}", e);
                exit(1);
            }
        }
    }
}

struct Sizes {
    /// Size of the input schema as a string
    text: usize,
    /// Size of the parsed schema
    gql: usize,
    /// Size of the input schema
    input: usize,
    /// Size of the API schema
    api: usize,
    /// Size of the API schema as a string
    api_text: usize,
    /// Time to parse the schema as an input and an API schema
    time: Duration,
}

struct Sizer {
    first: AtomicBool,
}

impl Sizer {
    fn size<T, F: Fn() -> Result<T>>(&self, f: F) -> Result<(usize, T)> {
        f()?;
        ALLOCATED.store(0, SeqCst);
        let res = f()?;
        let end = ALLOCATED.load(SeqCst);
        Ok((end, res))
    }

    fn collect_sizes(&self, raw: &str, name: &str) -> Result<Sizes> {
        // Prime possible lazy_statics etc.
        let start = Instant::now();
        let id = parse(raw, name, true)?;
        let elapsed = start.elapsed();
        let txt_size = raw.len();
        let (gql_size, _) = self.size(|| {
            parse_schema(raw)
                .map(|v| v.into_static())
                .map_err(Into::into)
        })?;
        let (input_size, input_schema) =
            self.size(|| InputSchema::parse_latest(raw, id.clone()).map_err(Into::into))?;
        let (api_size, api) = self.size(|| input_schema.api_schema().map_err(Into::into))?;
        let api_text = api.document().to_string().len();
        Ok(Sizes {
            gql: gql_size,
            text: txt_size,
            input: input_size,
            api: api_size,
            api_text,
            time: elapsed,
        })
    }
}

impl Runner for Sizer {
    fn run(&self, raw: &str, name: &str, _api: bool) {
        if self.first.swap(false, SeqCst) {
            println!("name,raw,gql,input,api,api_text,time_ns");
        }
        match self.collect_sizes(raw, name) {
            Ok(sizes) => {
                println!(
                    "{name},{},{},{},{},{},{}",
                    sizes.text,
                    sizes.gql,
                    sizes.input,
                    sizes.api,
                    sizes.api_text,
                    sizes.time.as_nanos()
                );
            }
            Err(e) => {
                eprintln!("Error: {}", e);
                exit(1);
            }
        }
    }
}

pub fn main() {
    // Allow fulltext search in schemas
    std::env::set_var("GRAPH_ALLOW_NON_DETERMINISTIC_FULLTEXT_SEARCH", "true");

    let opt = Opts::parse();

    let runner: Box<dyn Runner> = match opt.mode {
        RunMode::Validate => Box::new(Validator),
        RunMode::Size => Box::new(Sizer {
            first: AtomicBool::new(true),
        }),
    };

    if opt.batch {
        for schema in &opt.schemas {
            eprintln!("Validating schemas from {schema}");
            let file = File::open(schema).expect("file exists");
            let rdr = BufReader::new(file);
            for line in rdr.lines() {
                let line = line.expect("invalid line").replace("\\\\", "\\");
                let entry = serde_json::from_str::<Entry>(&line).expect("line is valid json");

                let raw = &entry.schema;
                let name = format!("sgd{}", entry.id);
                runner.run(raw, &name, opt.api);
            }
        }
    } else {
        for schema in &opt.schemas {
            eprintln!("Validating schema from {schema}");
            let raw = std::fs::read_to_string(schema).expect("file exists");
            runner.run(&raw, schema, opt.api);
        }
    }
}
