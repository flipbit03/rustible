//! Every built-in filter, test and global function `template!` promises
//! (T1), one test each, rendered through the macro with Rustible's
//! environment: a built-in that goes missing (a minijinja feature turned
//! off, an upgrade that drops one) fails its test by name. The list is the
//! one in `template!`'s rustdoc; a test added here belongs there too.

use rustible::prelude::*;

/// One `#[test]` per case: the template renders to the expected text. The
/// empty spread lets a case name a variable it never passes (`missing`), to
/// exercise `default` and `defined`.
macro_rules! renders {
    ($($name:ident: $template:literal => $expected:expr;)*) => {
        $(
            #[test]
            fn $name() {
                let none = rustible::json!({});
                assert_eq!(template!($template, ..&none).unwrap(), $expected);
            }
        )*
    };
}

// ---- filters ----

renders! {
    filter_abs: "{{ -3|abs }}" => "3";
    filter_attr: "{{ {'a': 1}|attr('a') }}" => "1";
    filter_batch: "{{ [1, 2, 3]|batch(2)|list }}" => "[[1, 2], [3]]";
    filter_bool: "{{ 'yes'|bool }} {{ 0|bool }}" => "True False";
    filter_capitalize: "{{ 'hello WORLD'|capitalize }}" => "Hello world";
    filter_chain: "{{ [1, 2]|chain([3])|list }}" => "[1, 2, 3]";
    filter_count: "{{ [1, 2, 3]|count }}" => "3";
    filter_d: "{{ missing|d('fallback') }}" => "fallback";
    filter_default: "{{ missing|default('fallback') }} {{ ''|default('e', true) }}" => "fallback e";
    filter_dictsort: "{% for k, v in {'b': 2, 'a': 1}|dictsort %}{{ k }}{{ v }}{% endfor %}" => "a1b2";
    filter_e: "{{ '<&>'|e }}" => "&lt;&amp;&gt;";
    filter_escape: "{{ '\"x\"'|escape }}" => "&quot;x&quot;";
    filter_first: "{{ [7, 8]|first }}" => "7";
    filter_float: "{{ '1.5'|float }}" => "1.5";
    filter_format: "{{ '%s-%s'|format('a', 1) }}" => "a-1";
    filter_groupby: "{% for g in [{'k': 'x', 'v': 1}, {'k': 'y', 'v': 2}, {'k': 'x', 'v': 3}]|groupby('k') %}{{ g.grouper }}:{{ g.list|map(attribute='v')|join(',') }};{% endfor %}" => "x:1,3;y:2;";
    filter_indent: "{{ 'a\nb'|indent(2) }}" => "a\n  b";
    filter_int: "{{ '42'|int + 1 }}" => "43";
    filter_items: "{% for k, v in {'a': 1}|items %}{{ k }}={{ v }}{% endfor %}" => "a=1";
    filter_join: "{{ ['a', 'b']|join(', ') }}" => "a, b";
    filter_last: "{{ [7, 8]|last }}" => "8";
    filter_length: "{{ 'abc'|length }} {{ [1]|length }}" => "3 1";
    filter_lines: "{{ 'a\nb'|lines|list }}" => "['a', 'b']";
    filter_list: "{{ 'ab'|list }}" => "['a', 'b']";
    filter_lower: "{{ 'ABC'|lower }}" => "abc";
    filter_map: "{{ [{'n': 1}, {'n': 2}]|map(attribute='n')|join }} {{ ['a']|map('upper')|join }}" => "12 A";
    filter_max: "{{ [3, 9, 1]|max }}" => "9";
    filter_min: "{{ [3, 9, 1]|min }}" => "1";
    filter_pprint: "{{ [1, 'a']|pprint }}" => "[\n    1,\n    'a',\n]";
    filter_reject: "{{ [1, 2, 3, 4]|reject('odd')|list }}" => "[2, 4]";
    filter_rejectattr: "{{ [{'on': true, 'n': 1}, {'on': false, 'n': 2}]|rejectattr('on')|map(attribute='n')|list }}" => "[2]";
    filter_replace: "{{ 'a-b-c'|replace('-', '+') }}" => "a+b+c";
    filter_reverse: "{{ [1, 2, 3]|reverse|list }} {{ 'ab'|reverse }}" => "[3, 2, 1] ba";
    filter_round: "{{ 2.567|round(1) }} {{ 3.5|round }} {{ 2.5|round }}" => "2.6 4.0 2.0";
    filter_safe: "{{ '<b>'|safe }}" => "<b>";
    filter_select: "{{ [1, 2, 3, 4]|select('even')|list }}" => "[2, 4]";
    filter_selectattr: "{{ [{'on': true, 'n': 1}, {'on': false, 'n': 2}]|selectattr('on')|map(attribute='n')|list }}" => "[1]";
    filter_slice: "{{ [1, 2, 3, 4, 5]|slice(2)|list }}" => "[[1, 2, 3], [4, 5]]";
    filter_sort: "{{ [3, 1, 2]|sort|list }} {{ [{'n': 2}, {'n': 1}]|sort(attribute='n')|map(attribute='n')|list }}" => "[1, 2, 3] [1, 2]";
    filter_split: "{{ 'a b  c'|split|list }} {{ 'a,b'|split(',')|list }}" => "['a', 'b', 'c'] ['a', 'b']";
    filter_string: "{{ (1|string) ~ 'x' }}" => "1x";
    filter_sum: "{{ [1, 2, 3]|sum }}" => "6";
    filter_title: "{{ 'hello world'|title }}" => "Hello World";
    filter_tojson: "{{ {'a': [1, true, none]}|tojson }}" => "{\"a\": [1, true, null]}";
    filter_trim: "{{ '  x  '|trim }}|{{ '--x--'|trim('-') }}" => "x|x";
    filter_unique: "{{ [1, 2, 1, 3]|unique|list }}" => "[1, 2, 3]";
    filter_upper: "{{ 'abc'|upper }}" => "ABC";
    filter_urlencode: "{{ 'a b&c'|urlencode }} {{ {'k': 'v w'}|urlencode }}" => "a%20b%26c k=v%20w";
    filter_zip: "{{ [1, 2]|zip(['a', 'b'])|list }}" => "[(1, 'a'), (2, 'b')]";
}

// ---- tests ----

renders! {
    test_boolean: "{{ true is boolean }} {{ 1 is boolean }}" => "True False";
    test_defined: "{{ missing is defined }} {{ 1 is defined }}" => "False True";
    test_divisibleby: "{{ 9 is divisibleby(3) }} {{ 9 is divisibleby(2) }}" => "True False";
    test_endingwith: "{{ 'abc' is endingwith('bc') }}" => "True";
    test_eq: "{{ 1 is eq(1) }} {{ 1 is equalto(2) }} {{ [1, 2]|select('==', 1)|list }}" => "True False [1]";
    test_escaped: "{{ ('x'|safe) is escaped }} {{ 'x' is escaped }}" => "True False";
    test_even: "{{ 2 is even }} {{ 3 is even }}" => "True False";
    test_false: "{{ false is false }} {{ 0 is false }}" => "True False";
    test_filter: "{{ 'upper' is filter }} {{ 'nope' is filter }}" => "True False";
    test_float: "{{ 1.0 is float }} {{ 1 is float }}" => "True False";
    test_ge: "{{ 2 is ge(2) }} {{ [1, 2, 3]|select('>=', 2)|list }}" => "True [2, 3]";
    test_gt: "{{ 3 is gt(2) }} {{ 3 is greaterthan(4) }} {{ [1, 3]|select('>', 2)|list }}" => "True False [3]";
    test_in: "{{ 2 is in([1, 2]) }} {{ 'z' is in('abc') }}" => "True False";
    test_int: "{{ 1 is int }} {{ 1.5 is int }}" => "True False";
    test_integer: "{{ 1 is integer }} {{ 'x' is integer }}" => "True False";
    test_iterable: "{{ [1] is iterable }} {{ 1 is iterable }}" => "True False";
    test_le: "{{ 2 is le(2) }} {{ [1, 2, 3]|select('<=', 2)|list }}" => "True [1, 2]";
    test_lower: "{{ 'abc' is lower }} {{ 'aBc' is lower }}" => "True False";
    test_lt: "{{ 1 is lt(2) }} {{ 3 is lessthan(2) }} {{ [1, 3]|select('<', 2)|list }}" => "True False [1]";
    test_mapping: "{{ {'a': 1} is mapping }} {{ [1] is mapping }}" => "True False";
    test_ne: "{{ 1 is ne(2) }} {{ [1, 2]|select('!=', 1)|list }}" => "True [2]";
    test_none: "{{ none is none }} {{ 0 is none }}" => "True False";
    test_number: "{{ 1.5 is number }} {{ '1' is number }}" => "True False";
    test_odd: "{{ 3 is odd }} {{ 2 is odd }}" => "True False";
    test_safe: "{{ ('x'|safe) is safe }} {{ 'x' is safe }}" => "True False";
    test_sameas: "{{ none is sameas(none) }} {{ 1 is sameas(true) }}" => "True False";
    test_sequence: "{{ [1] is sequence }} {{ {'a': 1} is sequence }}" => "True False";
    test_startingwith: "{{ 'abc' is startingwith('ab') }}" => "True";
    test_string: "{{ 'a' is string }} {{ 1 is string }}" => "True False";
    test_test: "{{ 'odd' is test }} {{ 'nope' is test }}" => "True False";
    test_true: "{{ true is true }} {{ 1 is true }}" => "True False";
    test_undefined: "{{ missing is undefined }} {{ 1 is undefined }}" => "True False";
    test_upper: "{{ 'ABC' is upper }} {{ 'AbC' is upper }}" => "True False";
}

// ---- global functions ----

renders! {
    function_range: "{{ range(3)|list }} {{ range(1, 7, 2)|list }}" => "[0, 1, 2] [1, 3, 5]";
    function_dict: "{{ dict(a=1, b=2)|dictsort }}" => "[('a', 1), ('b', 2)]";
    function_namespace: "{% set ns = namespace(n=0) %}{% for i in [1, 2, 3] %}{% set ns.n = ns.n + i %}{% endfor %}{{ ns.n }}" => "6";
}

#[test]
fn function_debug() {
    // `debug()` prints the context; its exact layout is minijinja's, so only
    // that it is there and shows a variable. It reads the context
    // dynamically, so a name only it shows is passed in a spread: as a named
    // argument the compile-time check would call it unused.
    let vars = rustible::json!({ "marker": "debug-marker" });
    let out = template!("{{ debug() }}", ..&vars).unwrap();
    assert!(out.contains("debug-marker"), "{out}");
}

// ---- tags beyond the plain ones ----

renders! {
    tag_loop_controls: "{% for i in range(10) %}{% if i == 1 %}{% continue %}{% endif %}{% if i == 3 %}{% break %}{% endif %}{{ i }}{% endfor %}" => "02";
    tag_loop_adjacent_items: "{% for i in [1, 2, 3] %}{{ loop.previtem|default('-') }}{{ i }}{{ loop.nextitem|default('-') }} {% endfor %}" => "-12 123 23- ";
    tag_call: "{% macro wrap() %}[{{ caller() }}]{% endmacro %}{% call wrap() %}in{% endcall %}" => "[in]";
    tag_filter_block: "{% filter upper %}shout{% endfilter %}" => "SHOUT";
    tag_with: "{% with x = 2 %}{{ x * 3 }}{% endwith %}" => "6";
    tag_do: "{% macro m() %}ignored{% endmacro %}{% do m() %}done" => "done";
    tag_raw: "{% raw %}{{ kept }}{% endraw %}" => "{{ kept }}";
    tag_set_block: "{% set x %}a{{ 1 + 1 }}{% endset %}{{ x }}" => "a2";
    tag_autoescape: "{% autoescape true %}{{ '<' }}{% endautoescape %}" => "&lt;";
}
