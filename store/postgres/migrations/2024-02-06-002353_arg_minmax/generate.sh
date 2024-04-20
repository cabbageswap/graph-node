#! /bin/bash

# Generate up and down migrations to define arg_min and arg_max functions
# for the types listed in `types`.
#
# The functions can all be used like
#
#     select first_int4((arg, value)) from t
#
# and return the `arg int4` for the smallest value `value int8`. If there
# are several rows with the smallest value, we try hard to return the first
# one, but that also depends on how Postgres calculates these
# aggregations. Note that the relation over which we are aggregating does
# not need to be ordered.
#
# Unfortunately, it is not possible to do this generically, so we have to
# monomorphize and define an aggregate for each data type that we want to
# use. The `value` is always an `int8`
#
# If changes to these functions are needed, copy this script to a new
# migration, change it and regenerate the up and down migrations

types="int4 int8 numeric"
dir=$(dirname $0)

read -d '' -r prelude <<'EOF'
-- This file was generated by generate.sh in this directory
set search_path = public;
EOF

read -d '' -r up_template <<'EOF'
create type public.@T@_and_value as (
  arg @T@,
  value int8
);

create or replace function arg_min_agg_@T@ (a @T@_and_value, b @T@_and_value)
  returns @T@_and_value
  language sql immutable strict parallel safe as
'select case when a.arg is null then b
             when b.arg is null then a
             when a.value <= b.value then a
             else b end';

create or replace function arg_max_agg_@T@ (a @T@_and_value, b @T@_and_value)
  returns @T@_and_value
  language sql immutable strict parallel safe as
'select case when a.arg is null then b
             when b.arg is null then a
             when a.value > b.value then a
             else b end';

create function arg_from_@T@_and_value(a @T@_and_value)
  returns @T@
  language sql immutable strict parallel safe as
'select a.arg';

create aggregate arg_min_@T@ (@T@_and_value) (
  sfunc    = arg_min_agg_@T@,
  stype    = @T@_and_value,
  finalfunc = arg_from_@T@_and_value,
  parallel = safe
);

comment on aggregate arg_min_@T@(@T@_and_value) is
'For ''select arg_min_@T@((arg, value)) from ..'' return the arg for the smallest value';

create aggregate arg_max_@T@ (@T@_and_value) (
  sfunc    = arg_max_agg_@T@,
  stype    = @T@_and_value,
  finalfunc = arg_from_@T@_and_value,
  parallel = safe
);

comment on aggregate arg_max_@T@(@T@_and_value) is
'For ''select arg_max_@T@((arg, value)) from ..'' return the arg for the largest value';
EOF

read -d '' -r down_template <<'EOF'
drop aggregate arg_min_@T@(@T@_and_value);
drop aggregate arg_max_@T@(@T@_and_value);
drop function arg_from_@T@_and_value(@T@_and_value);
drop function arg_max_agg_@T@(@T@_and_value, @T@_and_value);
drop function arg_min_agg_@T@(@T@_and_value, @T@_and_value);
drop type @T@_and_value;
EOF

echo "$prelude" > $dir/up.sql
for typ in $types
do
    echo "${up_template//@T@/$typ}" >> $dir/up.sql
done

echo "$prelude" > $dir/down.sql
for typ in $types
do
    echo "${down_template//@T@/$typ}" >> $dir/down.sql
done
