use itertools::Itertools;
use pest::iterators::Pair;
use pest::Parser;
use std::collections::BTreeMap;
use std::path::PathBuf;

use dhall_parser::{DhallParser, Rule};

use crate::*;

// This file consumes the parse tree generated by pest and turns it into
// our own AST. All those custom macros should eventually moved into
// their own crate because they are quite general and useful. For now they
// are here and hopefully you can figure out how they work.

use crate::ExprF::*;

type ParsedText = InterpolatedText<SubExpr<X, Import>>;
type ParsedTextContents = InterpolatedTextContents<SubExpr<X, Import>>;

pub type ParseError = pest::error::Error<Rule>;

pub type ParseResult<T> = Result<T, ParseError>;

#[derive(Debug)]
enum Either<A, B> {
    Left(A),
    Right(B),
}

impl crate::Builtin {
    pub fn parse(s: &str) -> Option<Self> {
        use crate::Builtin::*;
        match s {
            "Bool" => Some(Bool),
            "Natural" => Some(Natural),
            "Integer" => Some(Integer),
            "Double" => Some(Double),
            "Text" => Some(Text),
            "List" => Some(List),
            "Optional" => Some(Optional),
            "Some" => Some(OptionalSome),
            "None" => Some(OptionalNone),
            "Natural/build" => Some(NaturalBuild),
            "Natural/fold" => Some(NaturalFold),
            "Natural/isZero" => Some(NaturalIsZero),
            "Natural/even" => Some(NaturalEven),
            "Natural/odd" => Some(NaturalOdd),
            "Natural/toInteger" => Some(NaturalToInteger),
            "Natural/show" => Some(NaturalShow),
            "Integer/toDouble" => Some(IntegerToDouble),
            "Integer/show" => Some(IntegerShow),
            "Double/show" => Some(DoubleShow),
            "List/build" => Some(ListBuild),
            "List/fold" => Some(ListFold),
            "List/length" => Some(ListLength),
            "List/head" => Some(ListHead),
            "List/last" => Some(ListLast),
            "List/indexed" => Some(ListIndexed),
            "List/reverse" => Some(ListReverse),
            "Optional/fold" => Some(OptionalFold),
            "Optional/build" => Some(OptionalBuild),
            "Text/show" => Some(TextShow),
            _ => None,
        }
    }
}

pub fn custom_parse_error(pair: &Pair<Rule>, msg: String) -> ParseError {
    let msg =
        format!("{} while matching on:\n{}", msg, debug_pair(pair.clone()));
    let e = pest::error::ErrorVariant::CustomError { message: msg };
    pest::error::Error::new_from_span(e, pair.as_span())
}

fn debug_pair(pair: Pair<Rule>) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    fn aux(s: &mut String, indent: usize, prefix: String, pair: Pair<Rule>) {
        let indent_str = "| ".repeat(indent);
        let rule = pair.as_rule();
        let contents = pair.as_str();
        let mut inner = pair.into_inner();
        let mut first = true;
        while let Some(p) = inner.next() {
            if first {
                first = false;
                let last = inner.peek().is_none();
                if last && p.as_str() == contents {
                    let prefix = format!("{}{:?} > ", prefix, rule);
                    aux(s, indent, prefix, p);
                    continue;
                } else {
                    writeln!(
                        s,
                        r#"{}{}{:?}: "{}""#,
                        indent_str, prefix, rule, contents
                    )
                    .unwrap();
                }
            }
            aux(s, indent + 1, "".into(), p);
        }
        if first {
            writeln!(
                s,
                r#"{}{}{:?}: "{}""#,
                indent_str, prefix, rule, contents
            )
            .unwrap();
        }
    }
    aux(&mut s, 0, "".into(), pair);
    s
}

macro_rules! make_parser {
    (@pattern, rule, $name:ident) => (Rule::$name);
    (@pattern, rule_group, $name:ident) => (_);
    (@filter, rule) => (true);
    (@filter, rule_group) => (false);

    (@body,
        $pair:expr,
        $children:expr,
        rule!( $name:ident<$o:ty>; $($args:tt)* )
    ) => (
        make_parser!(@body,
            $pair,
            $children,
            rule!( $name<$o> as $name; $($args)* )
        )
    );
    (@body,
        $pair:expr,
        $children:expr,
        rule!(
            $name:ident<$o:ty>
            as $group:ident;
            captured_str!($x:pat) => $body:expr
        )
    ) => ({
        let $x = $pair.as_str();
        let res: $o = $body;
        Ok(ParsedValue::$group(res))
    });
    (@body,
        $pair:expr,
        $children:expr,
        rule!(
            $name:ident<$o:ty>
            as $group:ident;
            children!( $( [$($args:tt)*] => $body:expr ),* $(,)* )
        )
    ) => ({
        #[allow(unused_imports)]
        use ParsedValue::*;
        #[allow(unreachable_code)]
        let res: $o = iter_patterns::match_vec!($children;
            $( [$($args)*] => $body, )*
            [x..] => Err(
                format!("Unexpected children: {:?}", x.collect::<Vec<_>>())
            )?,
        ).ok_or_else(|| -> String { unreachable!() })?;
        Ok(ParsedValue::$group(res))
    });
    (@body, $pair:expr, $children:expr, rule_group!( $name:ident<$o:ty> )) => (
        unreachable!()
    );

    ($( $submac:ident!( $name:ident<$o:ty> $($args:tt)* ); )*) => (
        #[allow(non_camel_case_types, dead_code)]
        #[derive(Debug)]
        enum ParsedValue<'a> {
            $( $name($o), )*
        }

        fn parse_any<'a>(pair: Pair<'a, Rule>, children: Vec<ParsedValue<'a>>)
                -> Result<ParsedValue<'a>, String> {
            match pair.as_rule() {
                $(
                    make_parser!(@pattern, $submac, $name)
                    if make_parser!(@filter, $submac)
                    => make_parser!(@body, pair, children,
                                           $submac!( $name<$o> $($args)* ))
                    ,
                )*
                r => Err(format!("Unexpected {:?}", r)),
            }
        }
    );
}

// Non-recursive implementation to avoid stack overflows
fn do_parse<'a>(initial_pair: Pair<'a, Rule>) -> ParseResult<ParsedValue<'a>> {
    enum StackFrame<'a> {
        Unprocessed(Pair<'a, Rule>),
        Processed(Pair<'a, Rule>, usize),
    }
    use StackFrame::*;
    let mut pairs_stack: Vec<StackFrame> =
        vec![Unprocessed(initial_pair.clone())];
    let mut values_stack: Vec<ParsedValue> = vec![];
    while let Some(p) = pairs_stack.pop() {
        match p {
            Unprocessed(mut pair) => loop {
                let mut pairs: Vec<_> = pair.clone().into_inner().collect();
                let n_children = pairs.len();
                if n_children == 1 && can_be_shortcutted(pair.as_rule()) {
                    pair = pairs.pop().unwrap();
                    continue;
                } else {
                    pairs_stack.push(Processed(pair, n_children));
                    pairs_stack
                        .extend(pairs.into_iter().map(StackFrame::Unprocessed));
                    break;
                }
            },
            Processed(pair, n) => {
                let mut children: Vec<_> =
                    values_stack.split_off(values_stack.len() - n);
                children.reverse();
                let val = match parse_any(pair.clone(), children) {
                    Ok(v) => v,
                    Err(msg) => Err(custom_parse_error(&pair, msg))?,
                };
                values_stack.push(val);
            }
        }
    }
    Ok(values_stack.pop().unwrap())
}

// List of rules that can be shortcutted if they have a single child
fn can_be_shortcutted(rule: Rule) -> bool {
    use Rule::*;
    match rule {
        import_alt_expression
        | or_expression
        | plus_expression
        | text_append_expression
        | list_append_expression
        | and_expression
        | combine_expression
        | prefer_expression
        | combine_types_expression
        | times_expression
        | equal_expression
        | not_equal_expression
        | application_expression
        | selector_expression
        | annotated_expression => true,
        _ => false,
    }
}

make_parser! {
    rule!(EOI<()>; captured_str!(_) => ());

    rule!(simple_label<Label>;
        captured_str!(s) => Label::from(s.trim().to_owned())
    );
    rule!(quoted_label<Label>;
        captured_str!(s) => Label::from(s.trim().to_owned())
    );
    rule!(label<Label>; children!(
        [simple_label(l)] => l,
        [quoted_label(l)] => l,
    ));
    rule!(unreserved_label<Label>; children!(
        [label(l)] => {
            if crate::Builtin::parse(&String::from(&l)).is_some() {
                Err(
                    format!("Builtin names are not allowed as bound variables")
                )?
            }
            l
        },
    ));

    rule!(double_quote_literal<ParsedText>; children!(
        [double_quote_chunk(chunks)..] => {
            chunks.collect()
        }
    ));

    rule!(double_quote_chunk<ParsedTextContents>; children!(
        [interpolation(e)] => {
            InterpolatedTextContents::Expr(e)
        },
        [double_quote_escaped(s)] => {
            InterpolatedTextContents::Text(s)
        },
        [double_quote_char(s)] => {
            InterpolatedTextContents::Text(s.to_owned())
        },
    ));
    rule!(double_quote_escaped<String>;
        captured_str!(s) => {
            match s {
                "\"" => "\"".to_owned(),
                "$" => "$".to_owned(),
                "\\" => "\\".to_owned(),
                "/" => "/".to_owned(),
                "b" => "\u{0008}".to_owned(),
                "f" => "\u{000C}".to_owned(),
                "n" => "\n".to_owned(),
                "r" => "\r".to_owned(),
                "t" => "\t".to_owned(),
                _ => {
                    // "uXXXX"
                    use std::convert::TryFrom;
                    let c = u16::from_str_radix(&s[1..5], 16).unwrap();
                    let c = char::try_from(c as u32).unwrap();
                    std::iter::once(c).collect()
                }
            }
        }
    );
    rule!(double_quote_char<&'a str>;
        captured_str!(s) => s
    );

    rule!(end_of_line<()>; captured_str!(_) => ());

    rule!(single_quote_literal<ParsedText>; children!(
        [end_of_line(eol), single_quote_continue(lines)] => {
            let space = InterpolatedTextContents::Text(" ".to_owned());
            let newline = InterpolatedTextContents::Text("\n".to_owned());
            let min_indent = lines
                .iter()
                .map(|l| {
                    l.iter().rev().take_while(|c| **c == space).count()
                })
                .min()
                .unwrap();

            lines
                .into_iter()
                .rev()
                .map(|mut l| { l.split_off(l.len() - min_indent); l })
                .intersperse(vec![newline])
                .flat_map(|x| x.into_iter().rev())
                .collect::<ParsedText>()
        }
    ));
    rule!(single_quote_char<&'a str>;
        captured_str!(s) => s
    );
    rule!(escaped_quote_pair<&'a str>;
        captured_str!(_) => "''"
    );
    rule!(escaped_interpolation<&'a str>;
        captured_str!(_) => "${"
    );
    rule!(interpolation<ParsedExpr>; children!(
        [expression(e)] => e
    ));

    rule!(single_quote_continue<Vec<Vec<ParsedTextContents>>>; children!(
        [interpolation(c), single_quote_continue(lines)] => {
            let c = InterpolatedTextContents::Expr(c);
            let mut lines = lines;
            lines.last_mut().unwrap().push(c);
            lines
        },
        [escaped_quote_pair(c), single_quote_continue(lines)] => {
            let c = InterpolatedTextContents::Text(c.to_owned());
            let mut lines = lines;
            lines.last_mut().unwrap().push(c);
            lines
        },
        [escaped_interpolation(c), single_quote_continue(lines)] => {
            let c = InterpolatedTextContents::Text(c.to_owned());
            let mut lines = lines;
            lines.last_mut().unwrap().push(c);
            lines
        },
        [single_quote_char("\n"), single_quote_continue(lines)] => {
            let mut lines = lines;
            lines.push(vec![]);
            lines
        },
        [single_quote_char(c), single_quote_continue(lines)] => {
            let c = InterpolatedTextContents::Text(c.to_owned());
            let mut lines = lines;
            lines.last_mut().unwrap().push(c);
            lines
        },
        [] => {
            vec![vec![]]
        },
    ));

    rule!(NaN<()>; captured_str!(_) => ());
    rule!(minus_infinity_literal<()>; captured_str!(_) => ());
    rule!(plus_infinity_literal<()>; captured_str!(_) => ());

    rule!(double_literal<core::Double>;
        captured_str!(s) => {
            let s = s.trim();
            match s.parse::<f64>() {
                Ok(x) if x.is_infinite() =>
                    Err(format!("Overflow while parsing double literal '{}'", s))?,
                Ok(x) => NaiveDouble::from(x),
                Err(e) => Err(format!("{}", e))?,
            }
        }
    );

    rule!(natural_literal<core::Natural>;
        captured_str!(s) => {
            s.trim()
                .parse()
                .map_err(|e| format!("{}", e))?
        }
    );

    rule!(integer_literal<core::Integer>;
        captured_str!(s) => {
            s.trim()
                .parse()
                .map_err(|e| format!("{}", e))?
        }
    );

    rule!(unquoted_path_component<&'a str>; captured_str!(s) => s);
    rule!(quoted_path_component<&'a str>; captured_str!(s) => s);
    rule!(path_component<&'a str>; children!(
        [unquoted_path_component(s)] => s,
        [quoted_path_component(s)] => s,
    ));
    rule!(path<PathBuf>; children!(
        [path_component(components)..] => {
            components.collect()
        }
    ));

    rule_group!(local<(FilePrefix, PathBuf)>);

    rule!(parent_path<(FilePrefix, PathBuf)> as local; children!(
        [path(p)] => (FilePrefix::Parent, p)
    ));
    rule!(here_path<(FilePrefix, PathBuf)> as local; children!(
        [path(p)] => (FilePrefix::Here, p)
    ));
    rule!(home_path<(FilePrefix, PathBuf)> as local; children!(
        [path(p)] => (FilePrefix::Home, p)
    ));
    rule!(absolute_path<(FilePrefix, PathBuf)> as local; children!(
        [path(p)] => (FilePrefix::Absolute, p)
    ));

    rule!(scheme<Scheme>; captured_str!(s) => match s {
        "http" => Scheme::HTTP,
        "https" => Scheme::HTTPS,
        _ => unreachable!(),
    });

    rule!(http_raw<URL>; children!(
        [scheme(sch), authority(auth), path(p)] => URL {
            scheme: sch,
            authority: auth,
            path: p,
            query: None,
            headers: None,
        },
        [scheme(sch), authority(auth), path(p), query(q)] => URL {
            scheme: sch,
            authority: auth,
            path: p,
            query: Some(q),
            headers: None,
        },
    ));

    rule!(authority<String>; captured_str!(s) => s.to_owned());

    rule!(query<String>; captured_str!(s) => s.to_owned());

    rule!(http<URL>; children!(
        [http_raw(url)] => url,
        [http_raw(url), import_hashed(ih)] =>
            URL { headers: Some(Box::new(ih)), ..url },
    ));

    rule!(env<String>; children!(
        [bash_environment_variable(s)] => s,
        [posix_environment_variable(s)] => s,
    ));
    rule!(bash_environment_variable<String>; captured_str!(s) => s.to_owned());
    rule!(posix_environment_variable<String>; captured_str!(s) => s.to_owned());

    rule!(missing<()>; captured_str!(_) => ());

    rule!(import_type<ImportLocation>; children!(
        [missing(_)] => {
            ImportLocation::Missing
        },
        [env(e)] => {
            ImportLocation::Env(e)
        },
        [http(url)] => {
            ImportLocation::Remote(url)
        },
        [local((prefix, p))] => {
            ImportLocation::Local(prefix, p)
        },
    ));

    rule!(hash<Hash>; captured_str!(s) =>
        Hash {
            protocol: s.trim()[..6].to_owned(),
            hash: s.trim()[7..].to_owned(),
        }
    );

    rule!(import_hashed<ImportHashed>; children!(
        [import_type(location)] =>
            ImportHashed { location, hash: None },
        [import_type(location), hash(h)] =>
            ImportHashed { location, hash: Some(h) },
    ));

    rule_group!(expression<ParsedExpr>);

    rule!(Text<()>; captured_str!(_) => ());

    rule!(import<ParsedExpr> as expression; children!(
        [import_hashed(location_hashed)] => {
            bx(Embed(Import {
                mode: ImportMode::Code,
                location_hashed
            }))
        },
        [import_hashed(location_hashed), Text(_)] => {
            bx(Embed(Import {
                mode: ImportMode::RawText,
                location_hashed
            }))
        },
    ));

    rule!(lambda_expression<ParsedExpr> as expression; children!(
        [unreserved_label(l), expression(typ), expression(body)] => {
            bx(Lam(l, typ, body))
        }
    ));

    rule!(ifthenelse_expression<ParsedExpr> as expression; children!(
        [expression(cond), expression(left), expression(right)] => {
            bx(BoolIf(cond, left, right))
        }
    ));

    rule!(let_expression<ParsedExpr> as expression; children!(
        [let_binding(bindings).., expression(final_expr)] => {
            bindings.fold(
                final_expr,
                |acc, x| bx(Let(x.0, x.1, x.2, acc))
            )
        }
    ));

    rule!(let_binding<(Label, Option<ParsedExpr>, ParsedExpr)>; children!(
        [unreserved_label(name), expression(annot), expression(expr)] =>
            (name, Some(annot), expr),
        [unreserved_label(name), expression(expr)] =>
            (name, None, expr),
    ));

    rule!(forall_expression<ParsedExpr> as expression; children!(
        [unreserved_label(l), expression(typ), expression(body)] => {
            bx(Pi(l, typ, body))
        }
    ));

    rule!(arrow_expression<ParsedExpr> as expression; children!(
        [expression(typ), expression(body)] => {
            bx(Pi("_".into(), typ, body))
        }
    ));

    rule!(merge_expression<ParsedExpr> as expression; children!(
        [expression(x), expression(y), expression(z)] =>
            bx(Merge(x, y, Some(z))),
        [expression(x), expression(y)] =>
            bx(Merge(x, y, None)),
    ));

    rule!(List<()>; captured_str!(_) => ());
    rule!(Optional<()>; captured_str!(_) => ());

    rule!(empty_collection<ParsedExpr> as expression; children!(
        [List(_), expression(t)] => {
            bx(EmptyListLit(t))
        },
        [Optional(_), expression(t)] => {
            bx(EmptyOptionalLit(t))
        },
    ));

    rule!(non_empty_optional<ParsedExpr> as expression; children!(
        [expression(x), Optional(_), expression(t)] => {
            rc(Annot(rc(NEOptionalLit(x)), t))
        }
    ));

    rule!(import_alt_expression<ParsedExpr> as expression; children!(
        [expression(e)] => e,
        [expression(first), expression(rest)..] => {
            let o = crate::BinOp::ImportAlt;
            rest.fold(first, |acc, e| bx(BinOp(o, acc, e)))
        },
    ));
    rule!(or_expression<ParsedExpr> as expression; children!(
        [expression(e)] => e,
        [expression(first), expression(rest)..] => {
            let o = crate::BinOp::BoolOr;
            rest.fold(first, |acc, e| bx(BinOp(o, acc, e)))
        },
    ));
    rule!(plus_expression<ParsedExpr> as expression; children!(
        [expression(e)] => e,
        [expression(first), expression(rest)..] => {
            let o = crate::BinOp::NaturalPlus;
            rest.fold(first, |acc, e| bx(BinOp(o, acc, e)))
        },
    ));
    rule!(text_append_expression<ParsedExpr> as expression; children!(
        [expression(e)] => e,
        [expression(first), expression(rest)..] => {
            let o = crate::BinOp::TextAppend;
            rest.fold(first, |acc, e| bx(BinOp(o, acc, e)))
        },
    ));
    rule!(list_append_expression<ParsedExpr> as expression; children!(
        [expression(e)] => e,
        [expression(first), expression(rest)..] => {
            let o = crate::BinOp::ListAppend;
            rest.fold(first, |acc, e| bx(BinOp(o, acc, e)))
        },
    ));
    rule!(and_expression<ParsedExpr> as expression; children!(
        [expression(e)] => e,
        [expression(first), expression(rest)..] => {
            let o = crate::BinOp::BoolAnd;
            rest.fold(first, |acc, e| bx(BinOp(o, acc, e)))
        },
    ));
    rule!(combine_expression<ParsedExpr> as expression; children!(
        [expression(e)] => e,
        [expression(first), expression(rest)..] => {
            let o = crate::BinOp::Combine;
            rest.fold(first, |acc, e| bx(BinOp(o, acc, e)))
        },
    ));
    rule!(prefer_expression<ParsedExpr> as expression; children!(
        [expression(e)] => e,
        [expression(first), expression(rest)..] => {
            let o = crate::BinOp::Prefer;
            rest.fold(first, |acc, e| bx(BinOp(o, acc, e)))
        },
    ));
    rule!(combine_types_expression<ParsedExpr> as expression; children!(
        [expression(e)] => e,
        [expression(first), expression(rest)..] => {
            let o = crate::BinOp::CombineTypes;
            rest.fold(first, |acc, e| bx(BinOp(o, acc, e)))
        },
    ));
    rule!(times_expression<ParsedExpr> as expression; children!(
        [expression(e)] => e,
        [expression(first), expression(rest)..] => {
            let o = crate::BinOp::NaturalTimes;
            rest.fold(first, |acc, e| bx(BinOp(o, acc, e)))
        },
    ));
    rule!(equal_expression<ParsedExpr> as expression; children!(
        [expression(e)] => e,
        [expression(first), expression(rest)..] => {
            let o = crate::BinOp::BoolEQ;
            rest.fold(first, |acc, e| bx(BinOp(o, acc, e)))
        },
    ));
    rule!(not_equal_expression<ParsedExpr> as expression; children!(
        [expression(e)] => e,
        [expression(first), expression(rest)..] => {
            let o = crate::BinOp::BoolNE;
            rest.fold(first, |acc, e| bx(BinOp(o, acc, e)))
        },
    ));

    rule!(annotated_expression<ParsedExpr> as expression; children!(
        [expression(e)] => e,
        [expression(e), expression(annot)] => {
            bx(Annot(e, annot))
        },
    ));

    rule!(application_expression<ParsedExpr> as expression; children!(
        [expression(e)] => e,
        [expression(first), expression(second)] => {
            match first.as_ref() {
                Builtin(crate::Builtin::OptionalNone) =>
                    bx(EmptyOptionalLit(second)),
                Builtin(crate::Builtin::OptionalSome) =>
                    bx(NEOptionalLit(second)),
                _ => bx(App(first, vec![second])),
            }
        },
        [expression(first), expression(second), expression(rest)..] => {
            match first.as_ref() {
                Builtin(crate::Builtin::OptionalNone) =>
                    bx(App(bx(EmptyOptionalLit(second)),
                                 rest.collect())),
                Builtin(crate::Builtin::OptionalSome) =>
                    bx(App(bx(NEOptionalLit(second)),
                                 rest.collect())),
                _ => bx(App(first,
                                  std::iter::once(second)
                                    .chain(rest)
                                    .collect())),
            }
        },
    ));

    rule!(selector_expression<ParsedExpr> as expression; children!(
        [expression(e)] => e,
        [expression(first), selector(rest)..] => {
            rest.fold(first, |acc, e| match e {
                Either::Left(l) => bx(Field(acc, l)),
                Either::Right(ls) => bx(Projection(acc, ls)),
            })
        }
    ));

    rule!(selector<Either<Label, Vec<Label>>>; children!(
        [label(l)] => Either::Left(l),
        [labels(ls)] => Either::Right(ls),
    ));

    rule!(labels<Vec<Label>>; children!(
        [label(ls)..] => ls.collect(),
    ));

    rule!(literal_expression<ParsedExpr> as expression; children!(
        [double_literal(n)] => bx(DoubleLit(n)),
        [minus_infinity_literal(n)] =>
            bx(DoubleLit(std::f64::NEG_INFINITY.into())),
        [plus_infinity_literal(n)] =>
            bx(DoubleLit(std::f64::INFINITY.into())),
        [NaN(n)] => bx(DoubleLit(std::f64::NAN.into())),
        [natural_literal(n)] => bx(NaturalLit(n)),
        [integer_literal(n)] => bx(IntegerLit(n)),
        [double_quote_literal(s)] => bx(TextLit(s)),
        [single_quote_literal(s)] => bx(TextLit(s)),
        [expression(e)] => e,
    ));

    rule!(identifier<ParsedExpr> as expression; children!(
        [label(l), natural_literal(idx)] => {
            let name = String::from(&l);
            match crate::Builtin::parse(name.as_str()) {
                Some(b) => bx(Builtin(b)),
                None => match name.as_str() {
                    "True" => bx(BoolLit(true)),
                    "False" => bx(BoolLit(false)),
                    "Type" => bx(Const(crate::Const::Type)),
                    "Kind" => bx(Const(crate::Const::Kind)),
                    _ => bx(Var(V(l, idx))),
                }
            }
        },
        [label(l)] => {
            let name = String::from(&l);
            match crate::Builtin::parse(name.as_str()) {
                Some(b) => bx(Builtin(b)),
                None => match name.as_str() {
                    "True" => bx(BoolLit(true)),
                    "False" => bx(BoolLit(false)),
                    "Type" => bx(Const(crate::Const::Type)),
                    "Kind" => bx(Const(crate::Const::Kind)),
                    _ => bx(Var(V(l, 0))),
                }
            }
        },
    ));

    rule!(empty_record_literal<ParsedExpr> as expression;
        captured_str!(_) => bx(RecordLit(BTreeMap::new()))
    );

    rule!(empty_record_type<ParsedExpr> as expression;
        captured_str!(_) => bx(RecordType(BTreeMap::new()))
    );

    rule!(non_empty_record_type_or_literal<ParsedExpr> as expression; children!(
        [label(first_label), non_empty_record_type(rest)] => {
            let (first_expr, mut map) = rest;
            map.insert(first_label, first_expr);
            bx(RecordType(map))
        },
        [label(first_label), non_empty_record_literal(rest)] => {
            let (first_expr, mut map) = rest;
            map.insert(first_label, first_expr);
            bx(RecordLit(map))
        },
    ));

    rule!(non_empty_record_type
          <(ParsedExpr, BTreeMap<Label, ParsedExpr>)>; children!(
        [expression(expr), record_type_entry(entries)..] => {
            (expr, entries.collect())
        }
    ));

    rule!(record_type_entry<(Label, ParsedExpr)>; children!(
        [label(name), expression(expr)] => (name, expr)
    ));

    rule!(non_empty_record_literal
          <(ParsedExpr, BTreeMap<Label, ParsedExpr>)>; children!(
        [expression(expr), record_literal_entry(entries)..] => {
            (expr, entries.collect())
        }
    ));

    rule!(record_literal_entry<(Label, ParsedExpr)>; children!(
        [label(name), expression(expr)] => (name, expr)
    ));

    rule!(union_type_or_literal<ParsedExpr> as expression; children!(
        [empty_union_type(_)] => {
            bx(UnionType(BTreeMap::new()))
        },
        [non_empty_union_type_or_literal((Some((l, e)), entries))] => {
            bx(UnionLit(l, e, entries))
        },
        [non_empty_union_type_or_literal((None, entries))] => {
            bx(UnionType(entries))
        },
    ));

    rule!(empty_union_type<()>; captured_str!(_) => ());

    rule!(non_empty_union_type_or_literal
          <(Option<(Label, ParsedExpr)>, BTreeMap<Label, ParsedExpr>)>;
            children!(
        [label(l), expression(e), union_type_entries(entries)] => {
            (Some((l, e)), entries)
        },
        [label(l), expression(e), non_empty_union_type_or_literal(rest)] => {
            let (x, mut entries) = rest;
            entries.insert(l, e);
            (x, entries)
        },
        [label(l), expression(e)] => {
            let mut entries = BTreeMap::new();
            entries.insert(l, e);
            (None, entries)
        },
    ));

    rule!(union_type_entries<BTreeMap<Label, ParsedExpr>>; children!(
        [union_type_entry(entries)..] => entries.collect()
    ));

    rule!(union_type_entry<(Label, ParsedExpr)>; children!(
        [label(name), expression(expr)] => (name, expr)
    ));

    rule!(non_empty_list_literal<ParsedExpr> as expression; children!(
        [expression(items)..] => bx(NEListLit(items.collect()))
    ));

    rule!(final_expression<ParsedExpr> as expression; children!(
        [expression(e), EOI(_eoi)] => e
    ));
}

pub fn parse_expr(s: &str) -> ParseResult<ParsedExpr> {
    let mut pairs = DhallParser::parse(Rule::final_expression, s)?;
    let expr = do_parse(pairs.next().unwrap())?;
    assert_eq!(pairs.next(), None);
    match expr {
        ParsedValue::expression(e) => Ok(e),
        _ => unreachable!(),
    }
    // Ok(bx(BoolLit(false)))
}

#[test]
fn test_parse() {
    // let expr = r#"{ x = "foo", y = 4 }.x"#;
    // let expr = r#"(1 + 2) * 3"#;
    let expr = r#"(1) + 3 * 5"#;
    println!("{:?}", parse_expr(expr));
    match parse_expr(expr) {
        Err(e) => {
            println!("{:?}", e);
            println!("{}", e);
        }
        ok => println!("{:?}", ok),
    };
    // assert!(false);
}
