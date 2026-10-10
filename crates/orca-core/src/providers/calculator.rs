//! The calculator: one arithmetic answer, served as a result row.
//!
//! Typing `2*(3+4)` in the query bar has to produce one row reading `14`, with
//! `= 2*(3+4)` as its second line, and activating it has to put `14` on the
//! clipboard. Everything below is a pure function of the query string: no
//! locale, no thousands separators, no `%` as a percentage, no clock, no I/O.
//! That is what lets every rule here be asserted outright rather than eyeballed
//! in a running window.
//!
//! # Why a bare number is not an answer
//!
//! A user typing `5` is far more likely looking for a file, a folder, or an app
//! whose name contains a 5 than asking what five is — and the answer to "5" is
//! "5", so the row would say nothing and displace something real. [`evaluate`]
//! therefore requires at least one binary operator before it returns a value at
//! all.
//!
//! # Why arithmetic is hand-written rather than delegated
//!
//! There is no expression-evaluation crate in the dependency list, and there is
//! nothing here that needs one: a recursive-descent parser over six operators
//! is a hundred lines of pure code with no allocation on the hot path beyond
//! the token buffer. The alternative is a `[dependencies]` entry in a crate
//! whose whole layout is arranged around not having one (see
//! `docs/ARCHITECTURE.md`), bought to save a hundred lines of testable code.
//!
//! ```
//! use orca_core::providers::{evaluate, format_value, CalculatorProvider, QueryProvider};
//!
//! assert_eq!(evaluate("2*(3+4)"), Some(14.0));
//! assert_eq!(format_value(1.0 / 3.0), "0.333333333333");
//!
//! let rows = CalculatorProvider.respond("2*(3+4)");
//! assert_eq!(rows.len(), 1);
//! assert_eq!(rows[0].title, "14");
//! assert_eq!(rows[0].subtitle.as_deref(), Some("= 2*(3+4)"));
//! ```

use crate::model::{LaunchTarget, Source};

use super::{QueryProvider, RawResult};

/// Longest query this provider will parse, in bytes.
///
/// `respond` runs on every keystroke and a paste is one keystroke, so the cost
/// of a pathological expression has to be bounded by something other than the
/// user's patience. The bound also caps the parser's recursion: the deepest a
/// parse can nest is one `(` for every two bytes of input, which is nowhere
/// near a stack overflow.
pub const MAX_EXPRESSION_LEN: usize = 256;

/// Significant digits kept when rendering an answer.
///
/// Twelve sits past the point where `f64` noise is visible — `0.1 + 0.2`
/// differs from `0.3` in the seventeenth digit — and short enough that a row
/// still reads as a number rather than as a dump of the mantissa.
const SIGNIFICANT_DIGITS: u32 = 12;

/// Evaluates an arithmetic expression.
///
/// Supported: decimal literals, `+ - * / % ^`, unary `+` and `-`, and
/// parentheses. `^` binds tighter than `*` and `/`, which bind tighter than `+`
/// and `-`; `^` is right-associative (`2^3^2` is 512) and every level
/// associates left-to-right (`10-4-3` is 3).
///
/// `None` — not a row, not a panic — for anything that fails to parse, a
/// leading or trailing binary operator, unbalanced parentheses, empty or
/// whitespace-only input, division or modulo by zero, a non-finite result, and
/// any input over [`MAX_EXPRESSION_LEN`] bytes.
///
/// **A bare number is not an answer:** `5` and `3.14` are `None`. See the module
/// docs.
#[must_use]
pub fn evaluate(expression: &str) -> Option<f64> {
    if expression.len() > MAX_EXPRESSION_LEN {
        return None;
    }
    let mut parser = Parser::new(expression);
    let value = parser.expression()?;
    parser.accept(value)
}

/// Renders an answer the way a person wants to read it.
///
/// Round to about twelve significant digits, then trim the zeros a fixed-point
/// rendering always leaves behind, so `0.1 + 0.2` reads `0.3` and not
/// `0.30000000000000004`. Plain decimal throughout and never scientific
/// notation: a magnitude a person reached by typing is a magnitude they can read
/// in full, and `1e+21` is a rendering for a machine. `inf` and `NaN` are never
/// emitted either — [`evaluate`] refuses them before this is called, and the
/// fallback exists so the function's contract holds even for a caller that did
/// not check.
#[must_use]
pub fn format_value(value: f64) -> String {
    // Two cases that must never reach the formatter, for different reasons.
    // `-0.0` is a real `f64` and renders as `-0`, when the answer to anything
    // that cancels to zero is simply zero. A non-finite value is already
    // refused by [`evaluate`], so it is unreachable through the provider; the
    // fallback is a number-shaped string rather than `"inf"` or `"NaN"` so this
    // function's contract holds for a caller that skipped the check.
    if value == 0.0 || !value.is_finite() {
        return "0".to_owned();
    }
    let exponent = value.abs().log10().floor() as i32;
    let decimals = (SIGNIFICANT_DIGITS as i32 - 1 - exponent).max(0) as usize;
    let rendered = format!("{value:.decimals$}");
    trim_trailing_zeros(&rendered)
}

/// Serves the arithmetic answer.
///
/// Stateless, so one instance is enough and it can be a `const` at a call site
/// that never wants to build a set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CalculatorProvider;

impl QueryProvider for CalculatorProvider {
    fn name(&self) -> &str {
        "calculator"
    }

    fn respond(&self, query: &str) -> Vec<RawResult> {
        // Trimmed once, here, so the id, the subtitle, and the parsed text are
        // all derived from one string. A pasted `" 2*(3+4) "` would otherwise
        // key a row as `calc: 2*(3+4) ` and render `=  2*(3+4)`, and neither
        // matches what the user reads on screen.
        let query = query.trim();
        if query.is_empty() {
            return Vec::new();
        }
        // Before parsing, not after: the point of the bound is that a long paste
        // costs one `len` call rather than a tokenisation.
        if query.len() > MAX_EXPRESSION_LEN {
            return Vec::new();
        }
        let Some(value) = evaluate(query) else {
            return Vec::new();
        };
        let title = format_value(value);
        // `format_value` has a non-finite fallback, so this is unreachable
        // through `evaluate`. It is checked anyway: an empty title is a blank
        // row, and a blank row is worse than no row because it looks like a
        // bug in the ranking rather than a question that went unanswered.
        if title.is_empty() {
            return Vec::new();
        }
        vec![RawResult::new(
            format!("calc:{}", query.to_lowercase()),
            title.clone(),
            Source::Calculator,
            LaunchTarget::CopyToClipboard(title),
        )
        .with_subtitle(format!("= {query}"))
        .with_score(1.0)]
    }
}

/// Strips the trailing zeros a fixed-point rendering always leaves behind, and
/// the point it leaves behind them.
fn trim_trailing_zeros(rendered: &str) -> String {
    if !rendered.contains('.') {
        return rendered.to_owned();
    }
    let trimmed = rendered.trim_end_matches('0');
    let trimmed = trimmed.strip_suffix('.').unwrap_or(trimmed);
    trimmed.to_owned()
}

/// Refuses a result that is not finite.
///
/// IEEE arithmetic signals failure by *producing* `inf` or `NaN` rather than by
/// trapping: `1/0` is `inf`, `0/0` is `NaN`, `5%0` is `NaN`, and `9^9^9` is
/// `inf`. Letting either through means either a title reading `inf` or a row
/// carrying a number that is not a number, so every arithmetic step refuses
/// here rather than a caller having to remember to check at the end.
///
/// Every operand reaching an arithmetic step is already the result of this
/// function or a parsed literal, so a non-finite value can never be laundered
/// back into a finite one — `inf.powf(0.0)` is `1.0`, and that is the one
/// operation that could have done it.
fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

/// One token position, one shared token buffer, no lookahead allocation.
struct Parser {
    chars: Vec<char>,
    position: usize,
    /// Whether any binary operator was consumed.
    ///
    /// A bare number parses perfectly well and still has to be refused, so the
    /// grammar cannot express "this was not a question" — the parser has to
    /// remember that it saw an operator.
    saw_operator: bool,
}

impl Parser {
    fn new(expression: &str) -> Parser {
        Parser {
            chars: expression.chars().collect(),
            position: 0,
            saw_operator: false,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.position).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let found = self.peek();
        if found.is_some() {
            self.position += 1;
        }
        found
    }

    /// Whitespace between tokens is uninteresting and never participates in
    /// anything, so it is skipped wherever a token is expected.
    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.position += 1;
        }
    }

    /// The whole input must be one expression.
    ///
    /// Trailing junk is a parse error rather than ignored input: `1+2)` is two
    /// different questions, and answering the first half of an expression the
    /// user did not finish typing is worse than showing nothing.
    fn accept(&mut self, value: f64) -> Option<f64> {
        self.skip_whitespace();
        if self.position != self.chars.len() {
            return None;
        }
        self.saw_operator.then_some(value)
    }

    /// `expr := term (('+' | '-') term)*`
    fn expression(&mut self) -> Option<f64> {
        let mut value = self.term()?;
        loop {
            self.skip_whitespace();
            let operator = match self.peek() {
                Some('+') => |left: f64, right: f64| left + right,
                Some('-') => |left: f64, right: f64| left - right,
                _ => return Some(value),
            };
            self.bump();
            self.saw_operator = true;
            let right = self.term()?;
            value = finite(operator(value, right))?;
        }
    }

    /// `term := unary (('*' | '/' | '%') unary)*`
    fn term(&mut self) -> Option<f64> {
        let mut value = self.unary()?;
        loop {
            self.skip_whitespace();
            let operator = match self.peek() {
                Some('*') => |left: f64, right: f64| left * right,
                Some('/') => |left: f64, right: f64| left / right,
                Some('%') => |left: f64, right: f64| left % right,
                _ => return Some(value),
            };
            self.bump();
            self.saw_operator = true;
            let right = self.unary()?;
            value = finite(operator(value, right))?;
        }
    }

    /// Exactly one sign, then a power.
    ///
    /// One sign rather than a recursive `unary := ('+' | '-') unary | power`,
    /// because `--5` is a typo and not a number: refusing a doubled sign keeps
    /// `2--3` — which is a genuine subtraction of a negative — working while
    /// `--5` still fails.
    ///
    /// The sign applies to the whole power, so `-3^2` is `-9` and not `9`. That
    /// is the convention every calculator and every textbook uses, and it is
    /// also why the sign is consumed here rather than inside [`Parser::power`].
    fn unary(&mut self) -> Option<f64> {
        self.skip_whitespace();
        let sign = match self.peek() {
            Some('+') => 1.0,
            Some('-') => -1.0,
            _ => return self.power(),
        };
        self.bump();
        let value = self.power()?;
        finite(sign * value)
    }

    /// `power := atom ('^' unary)?`
    ///
    /// The exponent is parsed as a `unary` rather than a `power`, and that
    /// single choice is what makes `^` right-associative: the right-hand side
    /// can carry another `^`, so `2^3^2` is `2^(3^2)` and not `(2^3)^2`.
    fn power(&mut self) -> Option<f64> {
        let mut value = self.atom()?;
        self.skip_whitespace();
        if self.peek() != Some('^') {
            return Some(value);
        }
        self.bump();
        self.saw_operator = true;
        let exponent = self.unary()?;
        value = finite(value.powf(exponent))?;
        Some(value)
    }

    /// `atom := number | '(' expr ')'`
    fn atom(&mut self) -> Option<f64> {
        self.skip_whitespace();
        match self.peek() {
            Some('(') => {
                self.bump();
                let value = self.expression()?;
                self.skip_whitespace();
                if self.bump() != Some(')') {
                    return None;
                }
                Some(value)
            }
            Some(c) if c.is_ascii_digit() => self.number(),
            _ => None,
        }
    }

    /// A run of digits with at most one decimal point and at least one digit
    /// before it. Rust's `f64` parser accepts a trailing point, so `2.*3` is
    /// valid; `atom` only calls this after seeing a leading digit, so `.5` is
    /// not.
    ///
    /// No exponent notation: `1e3` is a rendering choice, not something a person
    /// types into a launcher's query bar, and accepting it would mean `2e` had
    /// to be an error rather than simply not a number.
    fn number(&mut self) -> Option<f64> {
        let start = self.position;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.position += 1;
        }
        if self.peek() == Some('.') {
            self.position += 1;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.position += 1;
            }
        }
        let text: String = self.chars[start..self.position].iter().collect();
        finite(text.parse::<f64>().ok()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::looks_like_path;

    /// The rendered answer, which is what a row and a test both care about.
    fn answer(query: &str) -> Option<String> {
        evaluate(query).map(format_value)
    }

    fn rows(query: &str) -> Vec<RawResult> {
        CalculatorProvider.respond(query)
    }

    fn only_row(query: &str) -> RawResult {
        let mut rows = rows(query);
        assert_eq!(rows.len(), 1, "{query:?} should be exactly one row");
        rows.remove(0)
    }

    #[test]
    fn follows_conventional_precedence_and_associativity() {
        // Each line is one rule someone will otherwise "simplify" away.
        assert_eq!(evaluate("2+3*4"), Some(14.0));
        assert_eq!(evaluate("2*3^2"), Some(18.0));
        assert_eq!(
            evaluate("2.*3"),
            Some(6.0),
            "a trailing decimal point is valid"
        );
        assert_eq!(evaluate("(2+3)*4"), Some(20.0));
        assert_eq!(
            evaluate("100/10/2"),
            Some(5.0),
            "same level is left to right"
        );
        assert_eq!(evaluate("10-4-3"), Some(3.0), "and so is subtraction");
        assert_eq!(evaluate("8/2*4"), Some(16.0));
        assert_eq!(evaluate("2^3^2"), Some(512.0), "^ is right-associative");
        assert_eq!(evaluate("-3^2"), Some(-9.0), "unary minus is looser than ^");
    }

    #[test]
    fn unary_minus_applies_to_the_whole_group() {
        assert_eq!(evaluate("-(3+4)"), Some(-7.0));
        assert_eq!(evaluate("-(2*3)"), Some(-6.0));
        assert_eq!(evaluate("+7"), None, "one sign is not a question");
    }

    #[test]
    fn a_doubled_sign_is_an_error_but_a_subtracted_negative_is_not() {
        assert_eq!(evaluate("--5"), None);
        assert_eq!(evaluate("+-5"), None);
        assert_eq!(evaluate("2--3"), Some(5.0));
    }

    #[test]
    fn a_bare_number_is_not_an_answer() {
        assert_eq!(evaluate("5"), None);
        assert_eq!(evaluate("3.14"), None);
        assert_eq!(evaluate("  7  "), None);
        assert_eq!(evaluate("(5)"), None);
        assert_eq!(evaluate("-5"), None);
    }

    #[test]
    fn division_and_modulo_by_zero_produce_no_row_at_all() {
        for query in ["1/0", "0/0", "5%0", "0%0", "1/(2-2)", "1%0"] {
            assert_eq!(evaluate(query), None, "{query:?}");
            assert!(rows(query).is_empty(), "{query:?} must not reach the UI");
        }
    }

    #[test]
    fn an_expression_that_overflows_produces_no_row() {
        // The parser checks finiteness at every step, so this is caught where
        // it happens rather than by a later check on a value nobody trusts.
        assert_eq!(evaluate("9^9^9"), None);
        assert!(rows("9^9^9").is_empty());
        assert_eq!(evaluate("9^9^9^9"), None);
    }

    #[test]
    fn renders_values_the_way_a_person_reads_them() {
        assert_eq!(format_value(1.0), "1");
        assert_eq!(format_value(1.5), "1.5");
        assert_eq!(format_value(-14.0), "-14");
        assert_eq!(format_value(0.0), "0");
        assert_eq!(format_value(-0.0), "0", "negative zero reads as zero");

        let third = answer("1/3").expect("1/3 has an answer");
        assert_eq!(third, "0.333333333333");
        assert!(!third.contains('e'), "{third} is not for a person to read");

        // The float-noise case, and the reason `SIGNIFICANT_DIGITS` exists.
        assert_eq!(answer("0.1+0.2").as_deref(), Some("0.3"));
        assert_ne!(answer("0.1+0.2").as_deref(), Some("0.30000000000000004"));
        assert_eq!(answer("2^0.5").as_deref(), Some("1.41421356237"));
        assert_eq!(answer("1/8").as_deref(), Some("0.125"));
        assert_eq!(answer("100/4").as_deref(), Some("25"));
    }

    #[test]
    fn nothing_here_renders_scientific_notation_or_a_non_finite_value() {
        for query in ["2^0.5", "1/3", "0.1+0.2", "7*6", "-8/3"] {
            let rendered = answer(query).expect("an answer");
            assert!(!rendered.contains('e'), "{query} rendered as {rendered}");
            assert!(!rendered.contains("inf"), "{query} rendered as {rendered}");
            assert!(!rendered.contains("NaN"), "{query} rendered as {rendered}");
        }
    }

    #[test]
    fn an_unanswerable_query_produces_no_row() {
        for query in [
            "",
            "   ",
            "\t\n",
            "notepad",
            "5",
            "3.14",
            "2+",
            "+2",
            "(1+2",
            "1+2)",
            "2..3",
            "*3",
            "()",
            "1 +* 2",
            "3 3",
            "1,5",
            "2^",
            "^2",
            "(())",
            "1/0",
            "9^9^9",
            "π",
            "C:\\Windows",
        ] {
            assert!(rows(query).is_empty(), "{query:?} must not reach the UI");
        }
    }

    #[test]
    fn a_computed_answer_is_one_copyable_row() {
        let rows = rows("2*(3+4)");
        assert_eq!(rows.len(), 1, "one answer, one row");
        let row = &rows[0];
        assert_eq!(row.id, "calc:2*(3+4)");
        assert_eq!(row.title, "14");
        assert_eq!(row.subtitle.as_deref(), Some("= 2*(3+4)"));
        assert_eq!(row.source, Source::Calculator);
        assert_eq!(row.score, 1.0, "the answer is exactly what was asked for");
        assert_eq!(
            row.target,
            LaunchTarget::CopyToClipboard("14".to_owned()),
            "there is nothing to open, so the answer goes to the clipboard"
        );
    }

    #[test]
    fn the_row_follows_the_query_as_the_user_reads_it() {
        let row = only_row("  2 + 3 ");
        assert_eq!(row.title, "5");
        assert_eq!(row.id, "calc:2 + 3");
        assert_eq!(row.subtitle.as_deref(), Some("= 2 + 3"));

        // The id is lowercased so the same expression keys the same frecency
        // row, but the answer and the subtitle keep the characters the user
        // typed — lowercasing either would misreport a value.
        let row = only_row("2*(3+4)");
        assert_eq!(row.id, "calc:2*(3+4)");
        assert_eq!(row.title, "14");
    }

    #[test]
    fn a_row_is_never_blank_and_never_carries_a_path() {
        for query in ["2*(3+4)", "1/3", "-(2+3)", "0.5*4"] {
            let row = only_row(query);
            assert!(!row.title.trim().is_empty(), "{query} produced a blank row");
            assert!(
                !looks_like_path(&row.title),
                "{query} rendered a path: {:?}",
                row.title
            );
            if let Some(subtitle) = &row.subtitle {
                assert!(
                    !looks_like_path(subtitle),
                    "{query} rendered a path: {subtitle:?}"
                );
            }
        }
    }

    #[test]
    fn a_very_long_query_is_refused_before_it_is_parsed() {
        // The same expression one operator shorter *is* an answer, which is
        // what makes the bound the thing that refused this rather than a
        // grammar that happens to reject a long string.
        let short: String = format!("{}1", "1+".repeat(63));
        assert!(short.len() < MAX_EXPRESSION_LEN);
        assert_eq!(only_row(&short).title, "64");

        let long: String = "1+".repeat(200);
        assert!(long.len() > MAX_EXPRESSION_LEN);
        assert!(rows(&long).is_empty());
        assert_eq!(evaluate(&long), None);

        // Whitespace alone is not an answer either, however long it is.
        let spaces: String = " ".repeat(MAX_EXPRESSION_LEN + 1);
        assert!(rows(&spaces).is_empty());
    }

    #[test]
    fn the_provider_names_itself_for_logs() {
        assert_eq!(CalculatorProvider.name(), "calculator");
    }
}
