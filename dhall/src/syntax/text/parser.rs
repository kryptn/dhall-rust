use itertools::Itertools;
use pest::prec_climber as pcl;
use pest::prec_climber::PrecClimber;
use std::collections::{BTreeMap, BTreeSet};
use std::iter::once;
use std::rc::Rc;

use pest_consume::{match_nodes, Parser};

use crate::operations::OpKind::*;
use crate::syntax::ExprKind::*;
use crate::syntax::NumKind::*;
use crate::syntax::{
    Double, Expr, FilePath, FilePrefix, Hash, ImportMode, ImportTarget,
    Integer, InterpolatedText, InterpolatedTextContents, Label, NaiveDouble,
    Natural, Scheme, Span, UnspannedExpr, URL, V,
};

// This file consumes the parse tree generated by pest and turns it into
// our own AST. All those custom macros should eventually moved into
// their own crate because they are quite general and useful. For now they
// are here and hopefully you can figure out how they work.

type ParsedText = InterpolatedText<Expr>;
type ParsedTextContents = InterpolatedTextContents<Expr>;
type ParseInput<'input> = pest_consume::Node<'input, Rule, Rc<str>>;

pub type ParseError = pest::error::Error<Rule>;
pub type ParseResult<T> = Result<T, ParseError>;

#[derive(Debug)]
enum Selector {
    Field(Label),
    Projection(BTreeSet<Label>),
    ProjectionByExpr(Expr),
}

fn input_to_span(input: ParseInput) -> Span {
    Span::make(input.user_data().clone(), input.as_pair().as_span())
}
fn spanned(input: ParseInput, x: UnspannedExpr) -> Expr {
    Expr::new(x, input_to_span(input))
}
fn spanned_union(span1: Span, span2: Span, x: UnspannedExpr) -> Expr {
    Expr::new(x, span1.union(&span2))
}

// Trim the shared indent off of a vec of lines, as defined by the Dhall semantics of multiline
// literals.
fn trim_indent(lines: &mut Vec<ParsedText>) {
    let is_indent = |c: char| c == ' ' || c == '\t';

    // There is at least one line so this is safe
    let last_line_head = lines.last().unwrap().head();
    let indent_chars = last_line_head
        .char_indices()
        .take_while(|(_, c)| is_indent(*c));
    let mut min_indent_idx = match indent_chars.last() {
        Some((i, _)) => i,
        // If there is no indent char, then no indent needs to be stripped
        None => return,
    };

    for line in lines.iter() {
        // Ignore empty lines
        if line.is_empty() {
            continue;
        }
        // Take chars from line while they match the current minimum indent.
        let indent_chars = last_line_head[0..=min_indent_idx]
            .char_indices()
            .zip(line.head().chars())
            .take_while(|((_, c1), c2)| c1 == c2);
        match indent_chars.last() {
            Some(((i, _), _)) => min_indent_idx = i,
            // If there is no indent char, then no indent needs to be stripped
            None => return,
        };
    }

    // Remove the shared indent from non-empty lines
    for line in lines.iter_mut() {
        if !line.is_empty() {
            line.head_mut().replace_range(0..=min_indent_idx, "");
        }
    }
}

/// Insert the expr into the map; in case of collision, create a RecursiveRecordMerge node.
fn insert_recordlit_entry(map: &mut BTreeMap<Label, Expr>, l: Label, e: Expr) {
    use crate::operations::BinOp::RecursiveRecordMerge;
    use std::collections::btree_map::Entry;
    match map.entry(l) {
        Entry::Vacant(entry) => {
            entry.insert(e);
        }
        Entry::Occupied(mut entry) => {
            let dummy = Expr::new(Num(Bool(false)), Span::Artificial);
            let other = entry.insert(dummy);
            entry.insert(Expr::new(
                Op(BinOp(RecursiveRecordMerge, other, e)),
                Span::DuplicateRecordFieldsSugar,
            ));
        }
    }
}

lazy_static::lazy_static! {
    static ref PRECCLIMBER: PrecClimber<Rule> = {
        use Rule::*;
        // In order of precedence
        let operators = vec![
            equivalent,
            import_alt,
            bool_or,
            natural_plus,
            text_append,
            list_append,
            bool_and,
            combine,
            prefer,
            combine_types,
            natural_times,
            bool_eq,
            bool_ne,
        ];
        PrecClimber::new(
            operators
                .into_iter()
                .map(|op| pcl::Operator::new(op, pcl::Assoc::Left))
                .collect(),
        )
    };
}

// Generate pest parser manually becaue otherwise we'd need to modify something outside of OUT_DIR
// and that's forbidden by docs.rs.
// This is equivalent to:
// ```
// #[derive(Parser)
// #[grammar = "..."]
// struct DhallParser;
// ```
include!(concat!(env!("OUT_DIR"), "/dhall_parser.rs"));

#[pest_consume::parser(parser = DhallParser, rule = Rule)]
impl DhallParser {
    fn EOI(_input: ParseInput) -> ParseResult<()> {
        Ok(())
    }

    #[alias(label)]
    fn simple_label(input: ParseInput) -> ParseResult<Label> {
        Ok(Label::from(input.as_str()))
    }
    #[alias(label)]
    fn quoted_label(input: ParseInput) -> ParseResult<Label> {
        Ok(Label::from(input.as_str()))
    }

    #[alias(label)]
    fn any_label_or_some(input: ParseInput) -> ParseResult<Label> {
        Ok(match_nodes!(input.into_children();
            [label(l)] => l,
            [Some_(_)] => Label::from("Some"),
        ))
    }

    fn double_quote_literal(input: ParseInput) -> ParseResult<ParsedText> {
        Ok(match_nodes!(input.into_children();
            [double_quote_chunk(chunks)..] => {
                chunks.collect()
            }
        ))
    }

    fn double_quote_chunk(
        input: ParseInput,
    ) -> ParseResult<ParsedTextContents> {
        Ok(match_nodes!(input.into_children();
            [expression(e)] => {
                InterpolatedTextContents::Expr(e)
            },
            [double_quote_char(s)] => {
                InterpolatedTextContents::Text(s)
            },
        ))
    }
    #[alias(double_quote_char)]
    fn double_quote_escaped(input: ParseInput) -> ParseResult<String> {
        Ok(match input.as_str() {
            "\"" => "\"".to_owned(),
            "$" => "$".to_owned(),
            "\\" => "\\".to_owned(),
            "/" => "/".to_owned(),
            "b" => "\u{0008}".to_owned(),
            "f" => "\u{000C}".to_owned(),
            "n" => "\n".to_owned(),
            "r" => "\r".to_owned(),
            "t" => "\t".to_owned(),
            // "uXXXX" or "u{XXXXX}"
            s => {
                use std::convert::TryInto;

                let s = &s[1..];
                let s = if &s[0..1] == "{" {
                    &s[1..s.len() - 1]
                } else {
                    s
                };

                if s.len() > 8 {
                    return Err(input.error(
                        "Escape sequences can't have more than 8 chars"
                            .to_string(),
                    ));
                }

                // pad with zeroes
                let s: String = std::iter::repeat('0')
                    .take(8 - s.len())
                    .chain(s.chars())
                    .collect();

                // `s` has length 8, so `bytes` has length 4
                let bytes: &[u8] = &hex::decode(s).unwrap();
                let i = u32::from_be_bytes(bytes.try_into().unwrap());
                match i {
                    0xD800..=0xDFFF => {
                        return Err(input.error(
                            "Escape sequences can't contain surrogate pairs"
                                .to_string(),
                        ))
                    }
                    0x0FFFE..=0x0FFFF
                    | 0x1FFFE..=0x1FFFF
                    | 0x2FFFE..=0x2FFFF
                    | 0x3FFFE..=0x3FFFF
                    | 0x4FFFE..=0x4FFFF
                    | 0x5FFFE..=0x5FFFF
                    | 0x6FFFE..=0x6FFFF
                    | 0x7FFFE..=0x7FFFF
                    | 0x8FFFE..=0x8FFFF
                    | 0x9FFFE..=0x9FFFF
                    | 0xAFFFE..=0xAFFFF
                    | 0xBFFFE..=0xBFFFF
                    | 0xCFFFE..=0xCFFFF
                    | 0xDFFFE..=0xDFFFF
                    | 0xEFFFE..=0xEFFFF
                    | 0xFFFFE..=0xFFFFF
                    | 0x10_FFFE..=0x10_FFFF => {
                        return Err(input.error(
                            "Escape sequences can't contain non-characters"
                                .to_string(),
                        ))
                    }
                    _ => {}
                }
                let c: char = i.try_into().unwrap();
                std::iter::once(c).collect()
            }
        })
    }
    fn double_quote_char(input: ParseInput) -> ParseResult<String> {
        Ok(input.as_str().to_owned())
    }

    fn single_quote_literal(input: ParseInput) -> ParseResult<ParsedText> {
        Ok(match_nodes!(input.into_children();
            [single_quote_continue(lines)] => {
                let newline: ParsedText = "\n".to_string().into();

                // Reverse lines and chars in each line
                let mut lines: Vec<ParsedText> = lines
                    .into_iter()
                    .rev()
                    .map(|l| l.into_iter().rev().collect::<ParsedText>())
                    .collect();

                trim_indent(&mut lines);

                Itertools::intersperse(lines.into_iter(), newline)
                    .flat_map(InterpolatedText::into_iter)
                    .collect::<ParsedText>()
            }
        ))
    }
    fn single_quote_char(input: ParseInput) -> ParseResult<&str> {
        Ok(input.as_str())
    }
    #[alias(single_quote_char)]
    fn escaped_quote_pair(_input: ParseInput) -> ParseResult<&str> {
        Ok("''")
    }
    #[alias(single_quote_char)]
    fn escaped_interpolation(_input: ParseInput) -> ParseResult<&str> {
        Ok("${")
    }

    // Returns a vec of lines in reversed order, where each line is also in reversed order.
    fn single_quote_continue(
        input: ParseInput,
    ) -> ParseResult<Vec<Vec<ParsedTextContents>>> {
        Ok(match_nodes!(input.into_children();
            [expression(e), single_quote_continue(lines)] => {
                let c = InterpolatedTextContents::Expr(e);
                let mut lines = lines;
                lines.last_mut().unwrap().push(c);
                lines
            },
            [single_quote_char(c), single_quote_continue(lines)] => {
                let mut lines = lines;
                if c == "\n" || c == "\r\n" {
                    lines.push(vec![]);
                } else {
                    // TODO: don't allocate for every char
                    let c = InterpolatedTextContents::Text(c.to_owned());
                    lines.last_mut().unwrap().push(c);
                }
                lines
            },
            [] => {
                vec![vec![]]
            },
        ))
    }

    #[alias(expression)]
    fn builtin(input: ParseInput) -> ParseResult<Expr> {
        let s = input.as_str();
        let e = match crate::builtins::Builtin::parse(s) {
            Some(b) => Builtin(b),
            None => match s {
                "True" => Num(Bool(true)),
                "False" => Num(Bool(false)),
                "Type" => Const(crate::syntax::Const::Type),
                "Kind" => Const(crate::syntax::Const::Kind),
                "Sort" => Const(crate::syntax::Const::Sort),
                _ => {
                    return Err(
                        input.error(format!("Unrecognized builtin: '{}'", s))
                    )
                }
            },
        };
        Ok(spanned(input, e))
    }

    #[alias(double_literal)]
    fn NaN(_input: ParseInput) -> ParseResult<Double> {
        Ok(std::f64::NAN.into())
    }
    #[alias(double_literal)]
    fn minus_infinity_literal(_input: ParseInput) -> ParseResult<Double> {
        Ok(std::f64::NEG_INFINITY.into())
    }
    #[alias(double_literal)]
    fn plus_infinity_literal(_input: ParseInput) -> ParseResult<Double> {
        Ok(std::f64::INFINITY.into())
    }

    #[alias(double_literal)]
    fn numeric_double_literal(input: ParseInput) -> ParseResult<Double> {
        let s = input.as_str().trim();
        match s.parse::<f64>() {
            Ok(x) if x.is_infinite() => Err(input.error(format!(
                "Overflow while parsing double literal '{}'",
                s
            ))),
            Ok(x) => Ok(NaiveDouble::from(x)),
            Err(e) => Err(input.error(format!("{}", e))),
        }
    }

    fn natural_literal(input: ParseInput) -> ParseResult<Natural> {
        let s = input.as_str().trim();
        if s.starts_with("0x") {
            let without_prefix = s.trim_start_matches("0x");
            u64::from_str_radix(without_prefix, 16)
                .map_err(|e| input.error(format!("{}", e)))
        } else {
            s.parse().map_err(|e| input.error(format!("{}", e)))
        }
    }

    fn integer_literal(input: ParseInput) -> ParseResult<Integer> {
        let s = input.as_str().trim();
        let (sign, rest) = (&s[0..1], &s[1..]);
        if rest.starts_with("0x") {
            let without_prefix =
                sign.to_owned() + rest.trim_start_matches("0x");
            i64::from_str_radix(&without_prefix, 16)
                .map_err(|e| input.error(format!("{}", e)))
        } else {
            s.parse().map_err(|e| input.error(format!("{}", e)))
        }
    }

    #[alias(expression, shortcut = true)]
    fn identifier(input: ParseInput) -> ParseResult<Expr> {
        Ok(match_nodes!(input.children();
            [variable(v)] => spanned(input, Var(v)),
            [expression(e)] => e,
        ))
    }

    fn variable(input: ParseInput) -> ParseResult<V> {
        Ok(match_nodes!(input.into_children();
            [label(l), natural_literal(idx)] => V(l, idx as usize),
            [label(l)] => V(l, 0),
        ))
    }

    #[alias(path_component)]
    fn unquoted_path_component(input: ParseInput) -> ParseResult<String> {
        Ok(input.as_str().to_string())
    }
    #[alias(path_component)]
    fn quoted_path_component(input: ParseInput) -> ParseResult<String> {
        #[rustfmt::skip]
        const RESERVED: &percent_encoding::AsciiSet =
            &percent_encoding::CONTROLS
            .add(b'=').add(b':').add(b'/').add(b'?')
            .add(b'#').add(b'[').add(b']').add(b'@')
            .add(b'!').add(b'$').add(b'&').add(b'\'')
            .add(b'(').add(b')').add(b'*').add(b'+')
            .add(b',').add(b';');
        Ok(input
            .as_str()
            .chars()
            .map(|c| {
                // Percent-encode ascii chars
                if c.is_ascii() {
                    percent_encoding::utf8_percent_encode(
                        &c.to_string(),
                        RESERVED,
                    )
                    .to_string()
                } else {
                    c.to_string()
                }
            })
            .collect())
    }
    fn path(input: ParseInput) -> ParseResult<FilePath> {
        Ok(match_nodes!(input.into_children();
            [path_component(components)..] => {
                FilePath { file_path: components.collect() }
            }
        ))
    }

    #[alias(import_type)]
    fn local(input: ParseInput) -> ParseResult<ImportTarget<Expr>> {
        Ok(match_nodes!(input.into_children();
            [local_path((prefix, p))] => ImportTarget::Local(prefix, p),
        ))
    }

    #[alias(local_path)]
    fn parent_path(input: ParseInput) -> ParseResult<(FilePrefix, FilePath)> {
        Ok(match_nodes!(input.into_children();
            [path(p)] => (FilePrefix::Parent, p)
        ))
    }
    #[alias(local_path)]
    fn here_path(input: ParseInput) -> ParseResult<(FilePrefix, FilePath)> {
        Ok(match_nodes!(input.into_children();
            [path(p)] => (FilePrefix::Here, p)
        ))
    }
    #[alias(local_path)]
    fn home_path(input: ParseInput) -> ParseResult<(FilePrefix, FilePath)> {
        Ok(match_nodes!(input.into_children();
            [path(p)] => (FilePrefix::Home, p)
        ))
    }
    #[alias(local_path)]
    fn absolute_path(input: ParseInput) -> ParseResult<(FilePrefix, FilePath)> {
        Ok(match_nodes!(input.into_children();
            [path(p)] => (FilePrefix::Absolute, p)
        ))
    }

    fn scheme(input: ParseInput) -> ParseResult<Scheme> {
        Ok(match input.as_str() {
            "http" => Scheme::HTTP,
            "https" => Scheme::HTTPS,
            _ => unreachable!(),
        })
    }

    fn http_raw(input: ParseInput) -> ParseResult<URL<Expr>> {
        Ok(match_nodes!(input.into_children();
            [scheme(sch), authority(auth), path_abempty(p)] => URL {
                scheme: sch,
                authority: auth,
                path: p,
                query: None,
                headers: None,
            },
            [scheme(sch), authority(auth), path_abempty(p), query(q)] => URL {
                scheme: sch,
                authority: auth,
                path: p,
                query: Some(q),
                headers: None,
            },
        ))
    }

    fn path_abempty(input: ParseInput) -> ParseResult<FilePath> {
        Ok(match_nodes!(input.into_children();
            [segment(segments)..] => {
                let mut file_path: Vec<_> = segments.collect();
                // An empty path normalizes to "/"
                if file_path.is_empty() {
                    file_path = vec!["".to_owned()];
                }
                FilePath { file_path }
            }
        ))
    }

    fn authority(input: ParseInput) -> ParseResult<String> {
        Ok(input.as_str().to_owned())
    }

    fn segment(input: ParseInput) -> ParseResult<String> {
        Ok(input.as_str().to_string())
    }

    fn query(input: ParseInput) -> ParseResult<String> {
        Ok(input.as_str().to_owned())
    }

    #[alias(import_type)]
    fn http(input: ParseInput) -> ParseResult<ImportTarget<Expr>> {
        Ok(ImportTarget::Remote(match_nodes!(input.into_children();
            [http_raw(url)] => url,
            [http_raw(url), expression(e)] => URL { headers: Some(e), ..url },
        )))
    }

    #[alias(import_type)]
    fn env(input: ParseInput) -> ParseResult<ImportTarget<Expr>> {
        Ok(match_nodes!(input.into_children();
            [environment_variable(v)] => ImportTarget::Env(v),
        ))
    }
    #[alias(environment_variable)]
    fn bash_environment_variable(input: ParseInput) -> ParseResult<String> {
        Ok(input.as_str().to_owned())
    }
    #[alias(environment_variable)]
    fn posix_environment_variable(input: ParseInput) -> ParseResult<String> {
        Ok(match_nodes!(input.into_children();
            [posix_environment_variable_character(chars)..] => {
                chars.collect()
            },
        ))
    }
    fn posix_environment_variable_character(
        input: ParseInput,
    ) -> ParseResult<&str> {
        Ok(match input.as_str() {
            "\\\"" => "\"",
            "\\\\" => "\\",
            "\\a" => "\u{0007}",
            "\\b" => "\u{0008}",
            "\\f" => "\u{000C}",
            "\\n" => "\n",
            "\\r" => "\r",
            "\\t" => "\t",
            "\\v" => "\u{000B}",
            s => s,
        })
    }

    #[alias(import_type)]
    fn missing(_input: ParseInput) -> ParseResult<ImportTarget<Expr>> {
        Ok(ImportTarget::Missing)
    }

    fn hash(input: ParseInput) -> ParseResult<Hash> {
        let s = input.as_str().trim();
        let protocol = &s[..6];
        let hash = &s[7..];
        if protocol != "sha256" {
            return Err(
                input.error(format!("Unknown hashing protocol '{}'", protocol))
            );
        }
        Ok(Hash::SHA256(hex::decode(hash).unwrap().into()))
    }

    fn import_hashed(
        input: ParseInput,
    ) -> ParseResult<crate::syntax::Import<Expr>> {
        use crate::syntax::Import;
        let mode = ImportMode::Code;
        Ok(match_nodes!(input.into_children();
            [import_type(location)] => Import { mode, location, hash: None },
            [import_type(location), hash(h)] => Import { mode, location, hash: Some(h) },
        ))
    }

    #[alias(import_mode)]
    fn Text(_input: ParseInput) -> ParseResult<ImportMode> {
        Ok(ImportMode::RawText)
    }
    #[alias(import_mode)]
    fn Location(_input: ParseInput) -> ParseResult<ImportMode> {
        Ok(ImportMode::Location)
    }

    #[alias(expression)]
    fn import(input: ParseInput) -> ParseResult<Expr> {
        use crate::syntax::Import;
        let import = match_nodes!(input.children();
            [import_hashed(imp)] => {
                Import { mode: ImportMode::Code, ..imp }
            },
            [import_hashed(imp), import_mode(mode)] => {
                Import { mode, ..imp }
            },
        );
        Ok(spanned(input, Import(import)))
    }

    fn lambda(_input: ParseInput) -> ParseResult<()> {
        Ok(())
    }
    fn forall(_input: ParseInput) -> ParseResult<()> {
        Ok(())
    }
    fn arrow(_input: ParseInput) -> ParseResult<()> {
        Ok(())
    }
    fn merge(_input: ParseInput) -> ParseResult<()> {
        Ok(())
    }
    fn assert(_input: ParseInput) -> ParseResult<()> {
        Ok(())
    }
    fn if_(_input: ParseInput) -> ParseResult<()> {
        Ok(())
    }
    fn toMap(_input: ParseInput) -> ParseResult<()> {
        Ok(())
    }

    #[alias(expression)]
    fn empty_list_literal(input: ParseInput) -> ParseResult<Expr> {
        Ok(match_nodes!(input.children();
            [expression(e)] => spanned(input, EmptyListLit(e)),
        ))
    }

    fn expression(input: ParseInput) -> ParseResult<Expr> {
        Ok(match_nodes!(input.children();
            [lambda(()), label(l), expression(typ),
                    arrow(()), expression(body)] => {
                spanned(input, Lam(l, typ, body))
            },
            [if_(()), expression(cond), expression(left),
                    expression(right)] => {
                spanned(input, Op(BoolIf(cond, left, right)))
            },
            [let_binding(bindings).., expression(final_expr)] => {
                bindings.rev().fold(
                    final_expr,
                    |acc, x| {
                        spanned_union(
                            acc.span(),
                            x.3,
                            Let(x.0, x.1, x.2, acc)
                        )
                    }
                )
            },
            [forall(()), label(l), expression(typ),
                    arrow(()), expression(body)] => {
                spanned(input, Pi(l, typ, body))
            },
            [expression(typ), arrow(()), expression(body)] => {
                spanned(input, Pi("_".into(), typ, body))
            },
            [merge(()), expression(x), expression(y), expression(z)] => {
                spanned(input, Op(Merge(x, y, Some(z))))
            },
            [assert(()), expression(x)] => {
                spanned(input, Assert(x))
            },
            [toMap(()), expression(x), expression(y)] => {
                spanned(input, Op(ToMap(x, Some(y))))
            },
            [expression(e), expression(annot)] => {
                spanned(input, Annot(e, annot))
            },
            [expression(e)] => e,
        ))
    }

    fn let_binding(
        input: ParseInput,
    ) -> ParseResult<(Label, Option<Expr>, Expr, Span)> {
        Ok(match_nodes!(input.children();
            [label(name), expression(annot), expression(expr)] =>
                (name, Some(annot), expr, input_to_span(input)),
            [label(name), expression(expr)] =>
                (name, None, expr, input_to_span(input)),
        ))
    }

    #[alias(expression, shortcut = true)]
    #[prec_climb(expression, PRECCLIMBER)]
    fn operator_expression(
        l: Expr,
        op: ParseInput,
        r: Expr,
    ) -> ParseResult<Expr> {
        use crate::operations::BinOp::*;
        use Rule::*;
        let op = match op.as_rule() {
            import_alt => ImportAlt,
            bool_or => BoolOr,
            natural_plus => NaturalPlus,
            text_append => TextAppend,
            list_append => ListAppend,
            bool_and => BoolAnd,
            combine => RecursiveRecordMerge,
            prefer => RightBiasedRecordMerge,
            combine_types => RecursiveRecordTypeMerge,
            natural_times => NaturalTimes,
            bool_eq => BoolEQ,
            bool_ne => BoolNE,
            equivalent => Equivalence,
            r => {
                return Err(op.error(format!("Rule {:?} isn't an operator", r)))
            }
        };

        Ok(spanned_union(l.span(), r.span(), Op(BinOp(op, l, r))))
    }

    fn Some_(_input: ParseInput) -> ParseResult<()> {
        Ok(())
    }

    #[alias(expression, shortcut = true)]
    fn with_expression(input: ParseInput) -> ParseResult<Expr> {
        Ok(match_nodes!(input.children();
            [expression(e)] => e,
            [expression(first), with_clause(clauses)..] => {
                clauses.fold(
                    first,
                    |acc, (labels, e)| {
                        spanned_union(
                            acc.span(),
                            e.span(),
                            Op(With(acc, labels, e))
                        )
                    }
                )
            },
        ))
    }

    fn with_clause(input: ParseInput) -> ParseResult<(Vec<Label>, Expr)> {
        Ok(match_nodes!(input.children();
            [label(labels).., expression(e)] => (labels.collect(), e),
        ))
    }

    #[alias(expression, shortcut = true)]
    fn application_expression(input: ParseInput) -> ParseResult<Expr> {
        Ok(match_nodes!(input.children();
            [expression(e)] => e,
            [expression(first), expression(rest)..] => {
                rest.fold(
                    first,
                    |acc, e| {
                        spanned_union(
                            acc.span(),
                            e.span(),
                            Op(App(acc, e))
                        )
                    }
                )
            },
        ))
    }

    #[alias(expression, shortcut = true)]
    fn first_application_expression(input: ParseInput) -> ParseResult<Expr> {
        Ok(match_nodes!(input.children();
            [Some_(()), expression(e)] => {
                spanned(input, SomeLit(e))
            },
            [merge(()), expression(x), expression(y)] => {
                spanned(input, Op(Merge(x, y, None)))
            },
            [toMap(()), expression(x)] => {
                spanned(input, Op(ToMap(x, None)))
            },
            [expression(e)] => e,
        ))
    }

    #[alias(expression, shortcut = true)]
    fn completion_expression(input: ParseInput) -> ParseResult<Expr> {
        Ok(match_nodes!(input.children();
            [expression(e)] => e,
            [expression(first), expression(rest)..] => {
                rest.fold(
                    first,
                    |acc, e| {
                        spanned_union(
                            acc.span(),
                            e.span(),
                            Op(Completion(acc, e)),
                        )
                    }
                )
            },
        ))
    }

    #[alias(expression, shortcut = true)]
    fn selector_expression(input: ParseInput) -> ParseResult<Expr> {
        Ok(match_nodes!(input.children();
            [expression(e)] => e,
            [expression(first), selector(rest)..] => {
                rest.fold(
                    first,
                    |acc, e| {
                        spanned_union(
                            acc.span(),
                            e.1,
                            match e.0 {
                                Selector::Field(l) => Op(Field(acc, l)),
                                Selector::Projection(ls) => Op(Projection(acc, ls)),
                                Selector::ProjectionByExpr(e) => Op(ProjectionByExpr(acc, e))
                            }
                        )
                    }
                )
            },
        ))
    }

    fn selector(input: ParseInput) -> ParseResult<(Selector, Span)> {
        let stor = match_nodes!(input.children();
            [label(l)] => Selector::Field(l),
            [labels(ls)] => Selector::Projection(ls),
            [expression(e)] => Selector::ProjectionByExpr(e),
        );
        Ok((stor, input_to_span(input)))
    }

    fn labels(input: ParseInput) -> ParseResult<BTreeSet<Label>> {
        Ok(match_nodes!(input.children();
            [label(ls)..] => {
                let mut set = BTreeSet::default();
                for l in ls {
                    if set.contains(&l) {
                        return Err(
                            input.error(format!("Duplicate field in projection"))
                        )
                    }
                    set.insert(l);
                }
                set
            },
        ))
    }

    #[alias(expression, shortcut = true)]
    fn primitive_expression(input: ParseInput) -> ParseResult<Expr> {
        Ok(match_nodes!(input.children();
            [double_literal(n)] => spanned(input, Num(Double(n))),
            [natural_literal(n)] => spanned(input, Num(Natural(n))),
            [integer_literal(n)] => spanned(input, Num(Integer(n))),
            [double_quote_literal(s)] => spanned(input, TextLit(s)),
            [single_quote_literal(s)] => spanned(input, TextLit(s)),
            [record_type_or_literal(e)] => spanned(input, e),
            [union_type(e)] => spanned(input, e),
            [expression(e)] => e,
        ))
    }

    fn record_type_or_literal(input: ParseInput) -> ParseResult<UnspannedExpr> {
        Ok(match_nodes!(input.children();
            [empty_record_literal(_)] => RecordLit(Default::default()),
            [non_empty_record_type(map)] => RecordType(map),
            [non_empty_record_literal(map)] => RecordLit(map),
            [] => RecordType(Default::default()),
        ))
    }

    fn empty_record_literal(input: ParseInput) -> ParseResult<()> {
        Ok(())
    }

    fn non_empty_record_type(
        input: ParseInput,
    ) -> ParseResult<BTreeMap<Label, Expr>> {
        Ok(match_nodes!(input.children();
            [record_type_entry(entries)..] => {
                let mut map = BTreeMap::default();
                for (l, t) in entries {
                    use std::collections::btree_map::Entry;
                    match map.entry(l) {
                        Entry::Occupied(_) => {
                            return Err(input.error(
                                "Duplicate field in record type"
                                    .to_string(),
                            ));
                        }
                        Entry::Vacant(e) => {
                            e.insert(t);
                        }
                    }
                }
                map
            },
        ))
    }

    fn record_type_entry(input: ParseInput) -> ParseResult<(Label, Expr)> {
        Ok(match_nodes!(input.into_children();
            [label(name), expression(expr)] => (name, expr)
        ))
    }

    fn non_empty_record_literal(
        input: ParseInput,
    ) -> ParseResult<BTreeMap<Label, Expr>> {
        Ok(match_nodes!(input.into_children();
            [record_literal_entry(entries)..] => {
                let mut map = BTreeMap::new();
                for (l, e) in entries {
                    insert_recordlit_entry(&mut map, l, e);
                }
                map
            }
        ))
    }

    fn record_literal_entry(input: ParseInput) -> ParseResult<(Label, Expr)> {
        Ok(match_nodes!(input.into_children();
            [label(name)] => {
                // Desugar record pun into a variable
                let expr = Expr::new(Var(name.clone().into()), Span::RecordPunSugar);
                (name, expr)
            },
            [label(name), expression(expr)] => (name, expr),
            [label(first_name), label(names).., expression(expr)] => {
                // Desugar dotted field syntax into nested records
                let expr = names.rev().fold(expr, |e, l| {
                    let map = once((l, e)).collect();
                    Expr::new(
                        RecordLit(map),
                        Span::DottedFieldSugar,
                    )
                });
                (first_name, expr)
            },
        ))
    }

    fn union_type(input: ParseInput) -> ParseResult<UnspannedExpr> {
        Ok(match_nodes!(input.children();
            [union_type_entry(entries)..] => {
                let mut map = BTreeMap::default();
                for (l, t) in entries {
                    use std::collections::btree_map::Entry;
                    match map.entry(l) {
                        Entry::Occupied(_) => {
                            return Err(input.error(
                                "Duplicate variant in union type"
                                    .to_string(),
                            ));
                        }
                        Entry::Vacant(e) => {
                            e.insert(t);
                        }
                    }
                }
                UnionType(map)
            },
        ))
    }

    fn union_type_entry(
        input: ParseInput,
    ) -> ParseResult<(Label, Option<Expr>)> {
        Ok(match_nodes!(input.children();
            [label(name), expression(expr)] => (name, Some(expr)),
            [label(name)] => (name, None),
        ))
    }

    #[alias(expression)]
    fn non_empty_list_literal(input: ParseInput) -> ParseResult<Expr> {
        Ok(match_nodes!(input.children();
            [expression(items)..] => spanned(
                input,
                NEListLit(items.collect())
            )
        ))
    }

    #[alias(expression)]
    fn final_expression(input: ParseInput) -> ParseResult<Expr> {
        Ok(match_nodes!(input.into_children();
            [expression(e), EOI(_)] => e
        ))
    }
}

pub fn parse_expr(input_str: &str) -> ParseResult<Expr> {
    let rc_input_str = input_str.to_string().into();
    let inputs = DhallParser::parse_with_userdata(
        Rule::final_expression,
        input_str,
        rc_input_str,
    )?;
    Ok(match_nodes!(<DhallParser>; inputs;
        [expression(e)] => e,
    ))
}

#[test]
#[cfg_attr(windows, ignore)]
// Check that the local copy of the grammar file is in sync with the one from dhall-lang.
fn test_grammar_files_in_sync() {
    use std::process::Command;

    let spec_abnf_path = "../dhall-lang/standard/dhall.abnf";
    let local_abnf_path = "src/syntax/text/dhall.abnf";

    let out = Command::new("git")
        .arg("diff")
        .arg("--no-index")
        .arg("--ignore-space-change")
        .arg("--color")
        .arg("--")
        .arg(local_abnf_path)
        .arg(spec_abnf_path)
        .output()
        .expect("failed to run `git diff` command");

    if !out.status.success() {
        let output = String::from_utf8_lossy(&out.stdout);
        panic!(
            "The local dhall.abnf file differs from the one from \
             dhall-lang!\n{}",
            output
        );
    }
}
