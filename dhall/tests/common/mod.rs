use pretty_assertions::assert_eq as assert_eq_pretty;

macro_rules! assert_eq_display {
    ($left:expr, $right:expr) => {{
        match (&$left, &$right) {
            (left_val, right_val) => {
                if !(*left_val == *right_val) {
                    panic!(
                        r#"assertion failed: `(left == right)`
 left: `{}`,
right: `{}`"#,
                        left_val, right_val
                    )
                }
            }
        }
    }};
}

#[macro_export]
macro_rules! make_spec_test {
    ($type:ident, $name:ident, $path:expr) => {
        #[test]
        #[allow(non_snake_case)]
        fn $name() {
            use crate::common::*;
            run_test($path, Feature::$type);
        }
    };
}

use dhall::*;
use dhall_core::*;
use std::path::PathBuf;

#[allow(dead_code)]
pub enum Feature {
    ParserSuccess,
    ParserFailure,
    Normalization,
    TypecheckSuccess,
    TypecheckFailure,
    TypeInferenceSuccess,
    TypeInferenceFailure,
}

fn read_dhall_file<'i>(file_path: &str) -> Result<Expr<X, X>, ImportError> {
    load_dhall_file(&PathBuf::from(file_path), true)
}

fn load_from_file_str<'i>(
    file_path: &str,
) -> Result<dhall::Parsed, ImportError> {
    Parsed::load_from_file(&PathBuf::from(file_path))
}

fn load_from_binary_file_str<'i>(
    file_path: &str,
) -> Result<dhall::Parsed, ImportError> {
    Parsed::load_from_binary_file(&PathBuf::from(file_path))
}

pub fn run_test(base_path: &str, feature: Feature) {
    use self::Feature::*;
    let base_path_prefix = match feature {
        ParserSuccess => "parser/success/",
        ParserFailure => "parser/failure/",
        Normalization => "normalization/success/",
        TypecheckSuccess => "typecheck/success/",
        TypecheckFailure => "typecheck/failure/",
        TypeInferenceSuccess => "type-inference/success/",
        TypeInferenceFailure => "type-inference/failure/",
    };
    let base_path =
        "../dhall-lang/tests/".to_owned() + base_path_prefix + base_path;
    match feature {
        ParserSuccess => {
            let expr_file_path = base_path.clone() + "A.dhall";
            let expected_file_path = base_path + "B.dhallb";
            let expr = load_from_file_str(&expr_file_path)
                .map_err(|e| println!("{}", e))
                .unwrap();

            let expected = load_from_binary_file_str(&expected_file_path)
                .map_err(|e| println!("{}", e))
                .unwrap();

            assert_eq_pretty!(expr, expected);

            // Round-trip pretty-printer
            let expr = Parsed::load_from_str(&expr.to_string()).unwrap();
            assert_eq!(expr, expected);
        }
        ParserFailure => {
            let file_path = base_path + ".dhall";
            let err = load_from_file_str(&file_path).unwrap_err();
            match err {
                ImportError::ParseError(_) => {}
                e => panic!("Expected parse error, got: {:?}", e),
            }
        }
        Normalization => {
            let expr_file_path = base_path.clone() + "A.dhall";
            let expected_file_path = base_path + "B.dhall";
            let expr = rc(read_dhall_file(&expr_file_path).unwrap());
            let expected = rc(read_dhall_file(&expected_file_path).unwrap());

            assert_eq_display!(normalize(expr), normalize(expected));
        }
        TypecheckFailure => {
            let file_path = base_path + ".dhall";
            let expr = rc(read_dhall_file(&file_path).unwrap());
            typecheck::type_of(expr).unwrap_err();
        }
        TypecheckSuccess => {
            // Many tests stack overflow in debug mode
            std::thread::Builder::new()
                .stack_size(4 * 1024 * 1024)
                .spawn(|| {
                    let expr_file_path = base_path.clone() + "A.dhall";
                    let expected_file_path = base_path + "B.dhall";
                    let expr = rc(read_dhall_file(&expr_file_path).unwrap());
                    let expected =
                        rc(read_dhall_file(&expected_file_path).unwrap());
                    typecheck::type_of(dhall::subexpr!(expr: expected))
                        .unwrap();
                })
                .unwrap()
                .join()
                .unwrap();
        }
        TypeInferenceFailure => {
            let file_path = base_path + ".dhall";
            let expr = rc(read_dhall_file(&file_path).unwrap());
            typecheck::type_of(expr).unwrap_err();
        }
        TypeInferenceSuccess => {
            let expr_file_path = base_path.clone() + "A.dhall";
            let expected_file_path = base_path + "B.dhall";
            let expr = rc(read_dhall_file(&expr_file_path).unwrap());
            let expected = rc(read_dhall_file(&expected_file_path).unwrap());
            assert_eq_display!(typecheck::type_of(expr).unwrap(), expected);
        }
    }
}
