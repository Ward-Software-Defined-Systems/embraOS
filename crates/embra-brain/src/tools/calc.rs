//! Arithmetic expression evaluator behind the `calculate` tool.
//!
//! In-tree replacement for the `meval` crate, which is unmaintained and
//! whose `nom 1.2.4` dependency trips a future-incompatibility lint on
//! every build. The grammar and the results are meval 0.2.0's, held by a
//! corpus of expressions whose expected values were taken from it while
//! both evaluators were in the tree (`calc_tests::GOLDEN`; the differential
//! run, including 80,000 generated expressions, is commit `3f1501d`):
//!
//! - binary `+ -` (precedence 1, left), `* / %` (2, left), `^` (4, right)
//! - unary `+ -` (3): `-2^2` is `-(2^2)`, `2^-2` is `2^(-2)`
//! - numbers `digits[.digits][(e|E)[+|-]digits]` — `.5` is not a number
//! - constants `pi`, `e`
//! - functions `sqrt exp ln abs sin cos tan asin acos atan sinh cosh tanh
//!   asinh acosh atanh floor ceil round signum` (one argument), `atan2`
//!   (two), `max` / `min` (one or more)
//! - no implicit multiplication; a name followed by `(` is always a call
//!
//! Three passes, like the original: tokenize (a two-state machine that
//! accepts only well-formed input), shunting-yard to reverse Polish
//! notation, evaluate. All three are loops — the expression comes from the
//! model, and nothing here recurses on its nesting depth. Where meval
//! could panic on malformed input, this returns an error.

use std::fmt;

/// Upper bound on the expression length. Every pass is linear in it.
pub(crate) const MAX_EXPRESSION_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CalcError {
    TooLong(usize),
    /// Byte offset of a token that is not allowed where it stands.
    UnexpectedToken(usize),
    /// Number of `)` missing at the end.
    MissingRParen(usize),
    /// The expression ends where an operand is expected.
    MissingArgument,
    UnknownVariable(String),
    UnknownFunction(String),
    WrongArity { name: String, expected: usize },
}

// The wording follows meval's: the model has seen these messages.
impl fmt::Display for CalcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CalcError::TooLong(n) => write!(
                f,
                "Parse error: expression is {n} bytes; the limit is {MAX_EXPRESSION_BYTES}."
            ),
            CalcError::UnexpectedToken(i) => {
                write!(f, "Parse error: Unexpected token at byte {i}.")
            }
            CalcError::MissingRParen(n) => write!(
                f,
                "Parse error: Missing {n} right parenthes{}.",
                if *n == 1 { "is" } else { "es" }
            ),
            CalcError::MissingArgument => {
                write!(f, "Parse error: Missing argument at the end of expression.")
            }
            CalcError::UnknownVariable(name) => {
                write!(f, "Evaluation error: unknown variable `{name}`.")
            }
            CalcError::UnknownFunction(name) => {
                write!(f, "Evaluation error: function `{name}`: Unknown function")
            }
            CalcError::WrongArity { name, expected } => write!(
                f,
                "Evaluation error: function `{name}`: Expected {expected} arguments"
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Plus,
    Minus,
    Times,
    Div,
    Rem,
    Pow,
}

#[derive(Debug, Clone, PartialEq)]
enum Token<'a> {
    Binary(Op),
    Unary(Op),
    LParen,
    RParen,
    Comma,
    Number(f64),
    Var(&'a str),
    /// A call, with the number of arguments once the closing `)` is seen.
    Func(&'a str, usize),
}

/// Evaluate `expression`.
pub(crate) fn eval(expression: &str) -> Result<f64, CalcError> {
    if expression.len() > MAX_EXPRESSION_BYTES {
        return Err(CalcError::TooLong(expression.len()));
    }
    let tokens = tokenize(expression)?;
    let rpn = to_rpn(tokens);
    eval_rpn(&rpn)
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

fn skip_space(s: &[u8], mut i: usize) -> usize {
    while i < s.len() && is_space(s[i]) {
        i += 1;
    }
    i
}

fn count_digits(s: &[u8], from: usize) -> usize {
    s[from.min(s.len())..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count()
}

/// Length of the number at `s[i..]`, or `None`. An `e` / `E` after the
/// mantissa commits to an exponent: `1e` and `1e+` are not numbers.
fn scan_number(s: &[u8], i: usize) -> Option<usize> {
    let int_digits = count_digits(s, i);
    if int_digits == 0 {
        return None;
    }
    let mut end = i + int_digits;
    if s.get(end) == Some(&b'.') {
        end += 1;
        end += count_digits(s, end);
    }
    if matches!(s.get(end), Some(b'e' | b'E')) {
        let mut exp = end + 1;
        if matches!(s.get(exp), Some(b'+' | b'-')) {
            exp += 1;
        }
        let exp_digits = count_digits(s, exp);
        if exp_digits == 0 {
            return None;
        }
        end = exp + exp_digits;
    }
    Some(end - i)
}

/// Length of the identifier at `s[i..]`: a letter or `_`, then letters,
/// digits or `_`.
fn scan_ident(s: &[u8], i: usize) -> Option<usize> {
    match s.get(i) {
        Some(b) if b.is_ascii_alphabetic() || *b == b'_' => Some(
            1 + s[i + 1..]
                .iter()
                .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_')
                .count(),
        ),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum Expect {
    /// An operand may start here: number, call, name, unary sign, `(`.
    Operand,
    /// An operand just ended: binary operator, `)`, or `,`.
    Operator,
}

#[derive(Clone, Copy, PartialEq)]
enum Paren {
    Group,
    Call,
}

fn tokenize(input: &str) -> Result<Vec<Token<'_>>, CalcError> {
    let s = input.as_bytes();
    let mut tokens = Vec::new();
    let mut parens: Vec<Paren> = Vec::new();
    let mut expect = Expect::Operand;
    let mut i = 0;

    while i < s.len() {
        i = skip_space(s, i);
        if i >= s.len() {
            // Only whitespace was left. It is not a token, so what the
            // expression still needs is decided below.
            break;
        }
        let at = i;
        match expect {
            Expect::Operand => {
                if let Some(len) = scan_number(s, i) {
                    // The slice is ASCII digits, `.`, `e`/`E` and a sign.
                    let value = input[i..i + len]
                        .parse::<f64>()
                        .map_err(|_| CalcError::UnexpectedToken(at))?;
                    tokens.push(Token::Number(value));
                    expect = Expect::Operator;
                    i += len;
                } else if let Some(len) = scan_ident(s, i) {
                    let name = &input[i..i + len];
                    let after = skip_space(s, i + len);
                    if s.get(after) == Some(&b'(') {
                        tokens.push(Token::Func(name, 0));
                        parens.push(Paren::Call);
                        i = after + 1;
                    } else {
                        tokens.push(Token::Var(name));
                        expect = Expect::Operator;
                        i += len;
                    }
                } else {
                    match s[i] {
                        b'+' => tokens.push(Token::Unary(Op::Plus)),
                        b'-' => tokens.push(Token::Unary(Op::Minus)),
                        b'(' => {
                            tokens.push(Token::LParen);
                            parens.push(Paren::Group);
                        }
                        _ => return Err(CalcError::UnexpectedToken(at)),
                    }
                    i += 1;
                }
            }
            Expect::Operator => {
                let op = match s[i] {
                    b'+' => Some(Op::Plus),
                    b'-' => Some(Op::Minus),
                    b'*' => Some(Op::Times),
                    b'/' => Some(Op::Div),
                    b'%' => Some(Op::Rem),
                    b'^' => Some(Op::Pow),
                    _ => None,
                };
                match (op, s[i], parens.last()) {
                    (Some(op), _, _) => {
                        tokens.push(Token::Binary(op));
                        expect = Expect::Operand;
                    }
                    (None, b')', Some(_)) => {
                        tokens.push(Token::RParen);
                        parens.pop();
                    }
                    (None, b',', Some(Paren::Call)) => {
                        tokens.push(Token::Comma);
                        expect = Expect::Operand;
                    }
                    _ => return Err(CalcError::UnexpectedToken(at)),
                }
                i += 1;
            }
        }
    }

    match expect {
        Expect::Operand => Err(CalcError::MissingArgument),
        Expect::Operator if !parens.is_empty() => Err(CalcError::MissingRParen(parens.len())),
        Expect::Operator => Ok(tokens),
    }
}

fn precedence(op: Op) -> (u32, bool) {
    // (precedence, right-associative)
    match op {
        Op::Plus | Op::Minus => (1, false),
        Op::Times | Op::Div | Op::Rem => (2, false),
        Op::Pow => (4, true),
    }
}

const UNARY_PRECEDENCE: u32 = 3;

/// Shunting-yard. The tokenizer accepts only well-formed input, so every
/// `)` and `,` finds its opener on the stack.
fn to_rpn(tokens: Vec<Token<'_>>) -> Vec<Token<'_>> {
    let mut output = Vec::with_capacity(tokens.len());
    let mut stack: Vec<Token<'_>> = Vec::new();

    for token in tokens {
        match token {
            Token::Number(_) | Token::Var(_) => output.push(token),
            Token::Unary(_) | Token::LParen | Token::Func(..) => stack.push(token),
            Token::Binary(op) => {
                let (prec, right) = precedence(op);
                while let Some(top) = stack.last() {
                    let top_prec = match top {
                        Token::Binary(o) => precedence(*o).0,
                        Token::Unary(_) => UNARY_PRECEDENCE,
                        _ => 0,
                    };
                    let pops = if right { prec < top_prec } else { prec <= top_prec };
                    if !pops {
                        break;
                    }
                    if let Some(t) = stack.pop() {
                        output.push(t);
                    }
                }
                stack.push(token);
            }
            Token::RParen => {
                while let Some(t) = stack.pop() {
                    match t {
                        Token::LParen => break,
                        Token::Func(name, commas) => {
                            output.push(Token::Func(name, commas + 1));
                            break;
                        }
                        other => output.push(other),
                    }
                }
            }
            Token::Comma => {
                while let Some(t) = stack.pop() {
                    match t {
                        Token::Func(name, commas) => {
                            stack.push(Token::Func(name, commas + 1));
                            break;
                        }
                        other => output.push(other),
                    }
                }
            }
        }
    }
    while let Some(t) = stack.pop() {
        output.push(t);
    }
    output
}

fn constant(name: &str) -> Option<f64> {
    match name {
        "pi" => Some(std::f64::consts::PI),
        "e" => Some(std::f64::consts::E),
        _ => None,
    }
}

fn call(name: &str, args: &[f64]) -> Result<f64, CalcError> {
    let unary: Option<fn(f64) -> f64> = match name {
        "sqrt" => Some(f64::sqrt),
        "exp" => Some(f64::exp),
        "ln" => Some(f64::ln),
        "abs" => Some(f64::abs),
        "sin" => Some(f64::sin),
        "cos" => Some(f64::cos),
        "tan" => Some(f64::tan),
        "asin" => Some(f64::asin),
        "acos" => Some(f64::acos),
        "atan" => Some(f64::atan),
        "sinh" => Some(f64::sinh),
        "cosh" => Some(f64::cosh),
        "tanh" => Some(f64::tanh),
        "asinh" => Some(f64::asinh),
        "acosh" => Some(f64::acosh),
        "atanh" => Some(f64::atanh),
        "floor" => Some(f64::floor),
        "ceil" => Some(f64::ceil),
        "round" => Some(f64::round),
        "signum" => Some(f64::signum),
        _ => None,
    };
    let arity = |expected: usize| CalcError::WrongArity {
        name: name.to_string(),
        expected,
    };
    if let Some(f) = unary {
        return match args {
            [x] => Ok(f(*x)),
            _ => Err(arity(1)),
        };
    }
    match name {
        "atan2" => match args {
            [y, x] => Ok(y.atan2(*x)),
            _ => Err(arity(2)),
        },
        "max" => Ok(args.iter().fold(f64::NEG_INFINITY, |m, x| m.max(*x))),
        "min" => Ok(args.iter().fold(f64::INFINITY, |m, x| m.min(*x))),
        _ => Err(CalcError::UnknownFunction(name.to_string())),
    }
}

fn eval_rpn(rpn: &[Token<'_>]) -> Result<f64, CalcError> {
    // The tokenizer guarantees the operand counts; a short stack would be
    // a bug here, reported as a parse error rather than a panic.
    let malformed = || CalcError::MissingArgument;
    let mut stack: Vec<f64> = Vec::with_capacity(16);

    for token in rpn {
        match token {
            Token::Number(n) => stack.push(*n),
            Token::Var(name) => match constant(name) {
                Some(v) => stack.push(v),
                None => return Err(CalcError::UnknownVariable((*name).to_string())),
            },
            Token::Binary(op) => {
                let right = stack.pop().ok_or_else(malformed)?;
                let left = stack.pop().ok_or_else(malformed)?;
                stack.push(match op {
                    Op::Plus => left + right,
                    Op::Minus => left - right,
                    Op::Times => left * right,
                    Op::Div => left / right,
                    Op::Rem => left % right,
                    Op::Pow => left.powf(right),
                });
            }
            Token::Unary(op) => {
                let x = stack.pop().ok_or_else(malformed)?;
                stack.push(if *op == Op::Minus { -x } else { x });
            }
            Token::Func(name, nargs) => {
                let first = stack.len().checked_sub(*nargs).ok_or_else(malformed)?;
                let value = call(name, &stack[first..])?;
                stack.truncate(first);
                stack.push(value);
            }
            Token::LParen | Token::RParen | Token::Comma => return Err(malformed()),
        }
    }

    match stack.as_slice() {
        [value] => Ok(*value),
        _ => Err(malformed()),
    }
}

#[cfg(test)]
mod calc_tests {
    use super::*;

    enum Want {
        Value(f64),
        Inf,
        NegInf,
        Nan,
        Err(&'static str),
    }

    /// Every operator, function, constant, number form and error path.
    /// The expected values are meval 0.2.0's: this table was generated
    /// from a run in which both evaluators agreed on each entry, values
    /// bit for bit and messages word for word. `calculate` rewrites `**`
    /// to `^` before evaluating, so the table writes `^`.
    ///
    /// A changed expectation is a changed tool behavior. Extend the table;
    /// do not edit entries to make a change pass.
    // The values are recorded outputs. Several are π, e, √2 and friends to
    // full precision, and naming them by constant would hide what the
    // evaluator is expected to return.
    #[allow(clippy::approx_constant)]
    #[rustfmt::skip]
    const GOLDEN: &[(&str, Want)] = &[
        ("1", Want::Value(1.0)),
        ("42", Want::Value(42.0)),
        ("007", Want::Value(7.0)),
        ("1.5", Want::Value(1.5)),
        ("1.", Want::Value(1.0)),
        ("1.e2", Want::Value(100.0)),
        ("1e3", Want::Value(1000.0)),
        ("1E3", Want::Value(1000.0)),
        ("1e+3", Want::Value(1000.0)),
        ("1e-3", Want::Value(0.001)),
        ("1.5e2", Want::Value(150.0)),
        ("0.1+0.2", Want::Value(0.30000000000000004)),
        ("0.1*3", Want::Value(0.30000000000000004)),
        ("9007199254740993", Want::Value(9007199254740992.0)),
        ("1e308*10", Want::Inf),
        ("1e-320", Want::Value(1e-320)),
        ("123456789*987654321", Want::Value(1.2193263111263526e+17)),
        (".5", Want::Err("Parse error: Unexpected token at byte 0.")),
        ("1e", Want::Err("Parse error: Unexpected token at byte 0.")),
        ("1e+", Want::Err("Parse error: Unexpected token at byte 0.")),
        ("1.2.3", Want::Err("Parse error: Unexpected token at byte 3.")),
        ("1..2", Want::Err("Parse error: Unexpected token at byte 2.")),
        ("1e5x", Want::Err("Parse error: Unexpected token at byte 3.")),
        ("1+2", Want::Value(3.0)),
        ("1-2", Want::Value(-1.0)),
        ("2*3", Want::Value(6.0)),
        ("7/2", Want::Value(3.5)),
        ("7%3", Want::Value(1.0)),
        ("-7%3", Want::Value(-1.0)),
        ("7%-3", Want::Value(1.0)),
        ("7.5%2", Want::Value(1.5)),
        ("2^10", Want::Value(1024.0)),
        ("2^0.5", Want::Value(1.4142135623730951)),
        ("2^3^2", Want::Value(512.0)),
        ("(2^3)^2", Want::Value(64.0)),
        ("1+2*3", Want::Value(7.0)),
        ("(1+2)*3", Want::Value(9.0)),
        ("1-2-3", Want::Value(-4.0)),
        ("8/4/2", Want::Value(1.0)),
        ("2*3%4", Want::Value(2.0)),
        ("10%4*2", Want::Value(4.0)),
        ("2+3*4^2", Want::Value(50.0)),
        ("2*3+4*5", Want::Value(26.0)),
        ("100/3", Want::Value(33.333333333333336)),
        ("1/3+1/3+1/3", Want::Value(1.0)),
        ("2^10-1", Want::Value(1023.0)),
        ("-1", Want::Value(-1.0)),
        ("+1", Want::Value(1.0)),
        ("--1", Want::Value(1.0)),
        ("-+-1", Want::Value(1.0)),
        ("-2^2", Want::Value(-4.0)),
        ("2^-2", Want::Value(0.25)),
        ("-2^-2", Want::Value(-0.25)),
        ("2*-3", Want::Value(-6.0)),
        ("2--3", Want::Value(5.0)),
        ("2+-3", Want::Value(-1.0)),
        ("-(1+2)", Want::Value(-3.0)),
        ("-2*3", Want::Value(-6.0)),
        ("- 2", Want::Value(-2.0)),
        ("2^-3^2", Want::Value(0.001953125)),
        ("-2*3^2", Want::Value(-18.0)),
        ("2^-3*4", Want::Value(0.5)),
        ("2*-3+4", Want::Value(-2.0)),
        ("+-+-2", Want::Value(2.0)),
        ("-pi", Want::Value(-3.141592653589793)),
        ("-sqrt(4)", Want::Value(-2.0)),
        ("2^-sqrt(4)", Want::Value(0.25)),
        (" 1 + 2 ", Want::Value(3.0)),
        ("1\t+\n2", Want::Value(3.0)),
        ("sqrt (4)", Want::Value(2.0)),
        ("max ( 1 , 2 )", Want::Value(2.0)),
        ("1 +\r\n 2", Want::Value(3.0)),
        ("pi", Want::Value(3.141592653589793)),
        ("e", Want::Value(2.718281828459045)),
        ("2*pi", Want::Value(6.283185307179586)),
        ("e^1", Want::Value(2.718281828459045)),
        ("pi*e", Want::Value(8.539734222673566)),
        ("pi^2", Want::Value(9.869604401089358)),
        ("e^pi", Want::Value(23.140692632779263)),
        ("sqrt(16)", Want::Value(4.0)),
        ("exp(1)", Want::Value(2.718281828459045)),
        ("ln(e)", Want::Value(1.0)),
        ("abs(-3.5)", Want::Value(3.5)),
        ("sin(pi/2)", Want::Value(1.0)),
        ("cos(0)", Want::Value(1.0)),
        ("tan(pi/4)", Want::Value(0.9999999999999999)),
        ("asin(1)", Want::Value(1.5707963267948966)),
        ("acos(1)", Want::Value(0.0)),
        ("atan(1)", Want::Value(0.7853981633974483)),
        ("sinh(1)", Want::Value(1.1752011936438014)),
        ("cosh(1)", Want::Value(1.5430806348152437)),
        ("tanh(1)", Want::Value(0.7615941559557649)),
        ("asinh(1)", Want::Value(0.881373587019543)),
        ("acosh(2)", Want::Value(1.3169578969248166)),
        ("atanh(0.5)", Want::Value(0.5493061443340548)),
        ("floor(-1.5)", Want::Value(-2.0)),
        ("ceil(-1.5)", Want::Value(-1.0)),
        ("round(2.5)", Want::Value(3.0)),
        ("round(-2.5)", Want::Value(-3.0)),
        ("round(2.4)", Want::Value(2.0)),
        ("signum(-3)", Want::Value(-1.0)),
        ("signum(0)", Want::Value(1.0)),
        ("signum(3)", Want::Value(1.0)),
        ("atan2(1,2)", Want::Value(0.4636476090008061)),
        ("atan2(-1,-1)", Want::Value(-2.356194490192345)),
        ("max(1)", Want::Value(1.0)),
        ("max(1,2,3)", Want::Value(3.0)),
        ("max(3,2,1)", Want::Value(3.0)),
        ("min(1)", Want::Value(1.0)),
        ("min(3,2,1)", Want::Value(1.0)),
        ("min(1,2,3)", Want::Value(1.0)),
        ("max(-1,-2)", Want::Value(-1.0)),
        ("min(1e3,1e-3)", Want::Value(0.001)),
        ("sqrt(abs(-16))", Want::Value(4.0)),
        ("max(sqrt(16), 2^3)", Want::Value(8.0)),
        ("max((1+2), 3)", Want::Value(3.0)),
        ("min(max(1,2),max(3,4))", Want::Value(2.0)),
        ("sqrt(sqrt(sqrt(256)))", Want::Value(2.0)),
        ("((((1))))", Want::Value(1.0)),
        ("(1+(2*(3-(4/5))))", Want::Value(5.4)),
        ("sin(cos(tan(1)))", Want::Value(0.013387802193205699)),
        ("max(1, max(2, max(3, 4)))", Want::Value(4.0)),
        ("2*(3+4)*(5-1)", Want::Value(56.0)),
        ("-(-(-(1)))", Want::Value(-1.0)),
        ("sqrt(-1)", Want::Nan),
        ("ln(0)", Want::NegInf),
        ("ln(-1)", Want::Nan),
        ("1/0", Want::Inf),
        ("-1/0", Want::NegInf),
        ("0/0", Want::Nan),
        ("acos(2)", Want::Nan),
        ("5%0", Want::Nan),
        ("0^0", Want::Value(1.0)),
        ("(-8)^(1/3)", Want::Nan),
        ("atanh(1)", Want::Inf),
        ("", Want::Err("Parse error: Missing argument at the end of expression.")),
        ("1+", Want::Err("Parse error: Missing argument at the end of expression.")),
        ("+", Want::Err("Parse error: Missing argument at the end of expression.")),
        ("*2", Want::Err("Parse error: Unexpected token at byte 0.")),
        ("1 2", Want::Err("Parse error: Unexpected token at byte 2.")),
        ("2(3)", Want::Err("Parse error: Unexpected token at byte 1.")),
        ("(1", Want::Err("Parse error: Missing 1 right parenthesis.")),
        ("((1)", Want::Err("Parse error: Missing 1 right parenthesis.")),
        ("1)", Want::Err("Parse error: Unexpected token at byte 1.")),
        ("()", Want::Err("Parse error: Unexpected token at byte 1.")),
        ("sqrt()", Want::Err("Parse error: Unexpected token at byte 5.")),
        ("sqrt(1,2)", Want::Err("Evaluation error: function `sqrt`: Expected 1 arguments")),
        ("atan2(1)", Want::Err("Evaluation error: function `atan2`: Expected 2 arguments")),
        ("atan2(1,2,3)", Want::Err("Evaluation error: function `atan2`: Expected 2 arguments")),
        ("foo(1)", Want::Err("Evaluation error: function `foo`: Unknown function")),
        ("x", Want::Err("Evaluation error: unknown variable `x`.")),
        ("sqrt", Want::Err("Evaluation error: unknown variable `sqrt`.")),
        ("pi(2)", Want::Err("Evaluation error: function `pi`: Unknown function")),
        ("1,2", Want::Err("Parse error: Unexpected token at byte 1.")),
        ("max(1,)", Want::Err("Parse error: Unexpected token at byte 6.")),
        ("max(,1)", Want::Err("Parse error: Unexpected token at byte 4.")),
        ("(1,2)", Want::Err("Parse error: Unexpected token at byte 2.")),
        ("max((1,2))", Want::Err("Parse error: Unexpected token at byte 6.")),
        ("2 pi", Want::Err("Parse error: Unexpected token at byte 2.")),
        ("1 + * 2", Want::Err("Parse error: Unexpected token at byte 4.")),
        ("3 $ 4", Want::Err("Parse error: Unexpected token at byte 2.")),
        ("a b", Want::Err("Parse error: Unexpected token at byte 2.")),
        ("1 +", Want::Err("Parse error: Missing argument at the end of expression.")),
        ("(", Want::Err("Parse error: Missing argument at the end of expression.")),
        (")", Want::Err("Parse error: Unexpected token at byte 0.")),
        ("((", Want::Err("Parse error: Missing argument at the end of expression.")),
        ("sqrt(", Want::Err("Parse error: Missing argument at the end of expression.")),
        ("max(1", Want::Err("Parse error: Missing 1 right parenthesis.")),
        ("max(1,2", Want::Err("Parse error: Missing 1 right parenthesis.")),
        ("1 ^", Want::Err("Parse error: Missing argument at the end of expression.")),
        ("^", Want::Err("Parse error: Unexpected token at byte 0.")),
        ("-", Want::Err("Parse error: Missing argument at the end of expression.")),
        ("2 * (3 + 4", Want::Err("Parse error: Missing 1 right parenthesis.")),
        ("foo(x)", Want::Err("Evaluation error: unknown variable `x`.")),
        ("sqrt(x)", Want::Err("Evaluation error: unknown variable `x`.")),
        ("_a", Want::Err("Evaluation error: unknown variable `_a`.")),
        ("a1_b2(3)", Want::Err("Evaluation error: function `a1_b2`: Unknown function")),
        ("1 = 2", Want::Err("Parse error: Unexpected token at byte 2.")),
        ("1 < 2", Want::Err("Parse error: Unexpected token at byte 2.")),
        ("2!", Want::Err("Parse error: Unexpected token at byte 1.")),
        ("√4", Want::Err("Parse error: Unexpected token at byte 0.")),
        ("1 + π", Want::Err("Parse error: Unexpected token at byte 4.")),
    ];

    /// Finite values compare within 1e-12 relative: `sin`, `exp`, `powf`
    /// and friends come from the platform's libm, which may differ in the
    /// last bit between hosts. A grammar or precedence regression moves a
    /// result by far more than that.
    fn close(got: f64, want: f64) -> bool {
        got == want || (got - want).abs() <= 1e-12 * got.abs().max(want.abs()).max(1.0)
    }

    #[test]
    fn golden_corpus() {
        let mut failures = Vec::new();
        for (expr, want) in GOLDEN {
            let got = eval(expr);
            let ok = match (want, &got) {
                (Want::Value(w), Ok(g)) => g.is_finite() && close(*g, *w),
                (Want::Inf, Ok(g)) => *g == f64::INFINITY,
                (Want::NegInf, Ok(g)) => *g == f64::NEG_INFINITY,
                (Want::Nan, Ok(g)) => g.is_nan(),
                (Want::Err(w), Err(g)) => g.to_string() == *w,
                _ => false,
            };
            if !ok {
                failures.push(format!("{expr:?}: got {got:?}"));
            }
        }
        assert!(failures.is_empty(), "{} of {} moved:\n{}", failures.len(), GOLDEN.len(), failures.join("\n"));
    }

    /// meval panicked on these (its tokenizer, on input that is only
    /// whitespace); the tool guards just the empty string.
    #[test]
    fn whitespace_only_input_is_an_error_not_a_panic() {
        for expr in [" ", "  ", "\t", "\n", " \r\n\t "] {
            assert_eq!(eval(expr), Err(CalcError::MissingArgument), "{expr:?}");
        }
    }

    #[test]
    fn over_long_input_is_refused() {
        let long = "1+".repeat(MAX_EXPRESSION_BYTES);
        assert!(matches!(eval(&long), Err(CalcError::TooLong(_))));
    }

    /// Depth is bounded by the length cap and costs heap, not stack.
    #[test]
    fn deep_nesting_does_not_recurse() {
        let depth = MAX_EXPRESSION_BYTES / 2 - 1;
        let expr = format!("{}1{}", "(".repeat(depth), ")".repeat(depth));
        assert_eq!(eval(&expr), Ok(1.0));
        let signs = format!("{}1", "-".repeat(MAX_EXPRESSION_BYTES - 1));
        assert_eq!(eval(&signs), Ok(-1.0));
    }
}
