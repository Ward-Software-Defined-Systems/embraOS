//! Arithmetic expression evaluator behind the `calculate` tool.
//!
//! In-tree replacement for the `meval` crate, which is unmaintained and
//! whose `nom 1.2.4` dependency trips a future-incompatibility lint on
//! every build. The grammar and the results are meval 0.2.0's, held by a
//! corpus of expressions whose expected values were taken from it
//! (`calc_tests`):
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

    /// Expressions that exercise every operator, function, constant,
    /// number form and error path. `calculate` rewrites `**` to `^` before
    /// evaluating, so the corpus writes `^`.
    pub(super) const CORPUS: &[&str] = &[
        // numbers
        "1", "42", "007", "1.5", "1.", "1.e2", "1e3", "1E3", "1e+3", "1e-3", "1.5e2",
        "0.1+0.2", "0.1*3", "9007199254740993", "1e308*10", "1e-320", "123456789*987654321",
        // not numbers
        ".5", "1e", "1e+", "1.2.3", "1..2", "1e5x",
        // binary operators, precedence, associativity
        "1+2", "1-2", "2*3", "7/2", "7%3", "-7%3", "7%-3", "7.5%2", "2^10", "2^0.5",
        "2^3^2", "(2^3)^2", "1+2*3", "(1+2)*3", "1-2-3", "8/4/2", "2*3%4", "10%4*2",
        "2+3*4^2", "2*3+4*5", "100/3", "1/3+1/3+1/3", "2^10-1",
        // unary signs
        "-1", "+1", "--1", "-+-1", "-2^2", "2^-2", "-2^-2", "2*-3", "2--3", "2+-3",
        "-(1+2)", "-2*3", "- 2", "2^-3^2", "-2*3^2", "2^-3*4", "2*-3+4", "+-+-2",
        "-pi", "-sqrt(4)", "2^-sqrt(4)",
        // whitespace
        " 1 + 2 ", "1\t+\n2", "sqrt (4)", "max ( 1 , 2 )", "1 +\r\n 2",
        // constants
        "pi", "e", "2*pi", "e^1", "pi*e", "pi^2", "e^pi",
        // functions
        "sqrt(16)", "exp(1)", "ln(e)", "abs(-3.5)", "sin(pi/2)", "cos(0)", "tan(pi/4)",
        "asin(1)", "acos(1)", "atan(1)", "sinh(1)", "cosh(1)", "tanh(1)", "asinh(1)",
        "acosh(2)", "atanh(0.5)", "floor(-1.5)", "ceil(-1.5)", "round(2.5)", "round(-2.5)",
        "round(2.4)", "signum(-3)", "signum(0)", "signum(3)", "atan2(1,2)", "atan2(-1,-1)",
        "max(1)", "max(1,2,3)", "max(3,2,1)", "min(1)", "min(3,2,1)", "min(1,2,3)",
        "max(-1,-2)", "min(1e3,1e-3)",
        // nesting
        "sqrt(abs(-16))", "max(sqrt(16), 2^3)", "max((1+2), 3)", "min(max(1,2),max(3,4))",
        "sqrt(sqrt(sqrt(256)))", "((((1))))", "(1+(2*(3-(4/5))))", "sin(cos(tan(1)))",
        "max(1, max(2, max(3, 4)))", "2*(3+4)*(5-1)", "-(-(-(1)))",
        // domain edges
        "sqrt(-1)", "ln(0)", "ln(-1)", "1/0", "-1/0", "0/0", "acos(2)", "5%0", "0^0",
        "(-8)^(1/3)", "atanh(1)",
        // errors
        "", "1+", "+", "*2", "1 2", "2(3)", "(1", "((1)", "1)", "()", "sqrt()",
        "sqrt(1,2)", "atan2(1)", "atan2(1,2,3)", "foo(1)", "x", "sqrt", "pi(2)", "1,2",
        "max(1,)", "max(,1)", "(1,2)", "max((1,2))", "2 pi", "1 + * 2", "3 $ 4",
        "a b", "1 +", "(", ")", "((", "sqrt(", "max(1", "max(1,2", "1 ^", "^", "-",
        "2 * (3 + 4", "foo(x)", "sqrt(x)", "_a", "a1_b2(3)", "1 = 2", "1 < 2", "2!",
        "\u{221a}4", "1 + \u{3c0}",
    ];

    fn same(a: f64, b: f64) -> bool {
        (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
    }

    /// E1 of the meval replacement: the two evaluators must agree on every
    /// corpus entry — value bit for bit, error message word for word. Where
    /// meval panics, this evaluator must return an error.
    #[test]
    fn agrees_with_meval_on_the_corpus() {
        let quiet = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let mut panicked = Vec::new();
        let mut failures = Vec::new();
        for expr in CORPUS {
            let theirs = std::panic::catch_unwind(|| meval::eval_str(expr));
            let ours = eval(expr);
            match (theirs, ours) {
                (Ok(Ok(t)), Ok(o)) if same(t, o) => {}
                (Ok(Err(t)), Err(o)) if t.to_string() == o.to_string() => {}
                (Err(_), Err(_)) => panicked.push(*expr),
                (t, o) => failures.push(format!(
                    "{expr:?}: meval={:?} ours={o:?}",
                    t.map(|r| r.map_err(|e| e.to_string())).map_err(|_| "PANIC")
                )),
            }
        }
        std::panic::set_hook(quiet);
        assert!(failures.is_empty(), "disagreements:\n{}", failures.join("\n"));
        // Printed with --nocapture: the table E2 freezes.
        for expr in CORPUS {
            match eval(expr) {
                Ok(v) => println!("GOLDEN {expr:?} => Ok({:#018x}) // {v}", v.to_bits()),
                Err(e) => println!("GOLDEN {expr:?} => Err({:?})", e.to_string()),
            }
        }
        println!("MEVAL_PANICS {panicked:?}");
    }

    /// Beyond the hand-picked corpus: fragments glued together at random
    /// (seeded, so the run is reproducible), most of them malformed. Same
    /// rule as above.
    #[test]
    fn agrees_with_meval_on_generated_expressions() {
        const FRAGMENTS: &[&str] = &[
            "0", "1", "2", "3.5", "10", "1e2", "1.", "2e-1", ".5", "1e", "pi", "e", "x",
            "+", "-", "*", "/", "%", "^", "(", ")", ",", " ", "  ", "\t",
            "sqrt(", "abs(", "ln(", "sin(", "max(", "min(", "atan2(", "floor(", "foo(",
            "round(", "exp(", "signum(", "$", "!", "=", "_", "a1",
        ];
        let quiet = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let mut state: u64 = 0x5eed_cafe_f00d_1234;
        let mut next = move |bound: usize| -> usize {
            // xorshift64*
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            (state.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 33) as usize % bound
        };
        let (mut agreed_ok, mut agreed_err, mut meval_panics) = (0u32, 0u32, 0u32);
        let mut failures = Vec::new();
        let mut panic_examples = Vec::new();
        for _ in 0..60_000 {
            let len = 1 + next(10);
            let expr: String = (0..len).map(|_| FRAGMENTS[next(FRAGMENTS.len())]).collect();
            let probe = expr.clone();
            let theirs = std::panic::catch_unwind(move || meval::eval_str(&probe));
            let ours = eval(&expr);
            match (theirs, ours) {
                (Ok(Ok(t)), Ok(o)) if same(t, o) => agreed_ok += 1,
                (Ok(Err(t)), Err(o)) if t.to_string() == o.to_string() => agreed_err += 1,
                (Err(_), Err(o)) => {
                    meval_panics += 1;
                    if panic_examples.len() < 5 {
                        panic_examples.push(format!("{expr:?} -> ours: {o}"));
                    }
                }
                (t, o) => {
                    if failures.len() < 20 {
                        failures.push(format!(
                            "{expr:?}: meval={:?} ours={o:?}",
                            t.map(|r| r.map_err(|e| e.to_string())).map_err(|_| "PANIC")
                        ));
                    }
                }
            }
        }
        std::panic::set_hook(quiet);
        println!("GENERATED ok={agreed_ok} err={agreed_err} meval_panics={meval_panics}");
        println!("PANIC_EXAMPLES {panic_examples:#?}");
        assert!(failures.is_empty(), "disagreements:\n{}", failures.join("\n"));
        assert!(agreed_ok > 1_000, "the generator produced too few valid expressions");
    }

    /// Well-formed expressions built from the grammar (seeded): random
    /// trees of operators, signs, calls with the right arity, constants
    /// and numbers, with whitespace sprinkled in. Every one must evaluate
    /// to the same bits in both evaluators.
    #[test]
    fn agrees_with_meval_on_generated_well_formed_expressions() {
        struct Gen(u64);
        impl Gen {
            fn next(&mut self, bound: usize) -> usize {
                self.0 ^= self.0 >> 12;
                self.0 ^= self.0 << 25;
                self.0 ^= self.0 >> 27;
                (self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 33) as usize % bound
            }
            fn ws(&mut self) -> &'static str {
                ["", "", "", " ", "  ", "\t"][self.next(6)]
            }
            fn atom(&mut self) -> String {
                const ATOMS: &[&str] = &[
                    "0", "1", "2", "3", "7", "10", "0.5", "1.25", "3.", "1e2", "2E-2",
                    "1.5e+1", "pi", "e", "100", "0.001",
                ];
                ATOMS[self.next(ATOMS.len())].to_string()
            }
            fn expr(&mut self, depth: usize) -> String {
                if depth == 0 {
                    return self.atom();
                }
                match self.next(10) {
                    0 | 1 => self.atom(),
                    2..=5 => {
                        let op = ["+", "-", "*", "/", "%", "^"][self.next(6)];
                        format!(
                            "{}{}{}{}{}",
                            self.expr(depth - 1),
                            self.ws(),
                            op,
                            self.ws(),
                            self.expr(depth - 1)
                        )
                    }
                    6 => format!("({}{}{})", self.ws(), self.expr(depth - 1), self.ws()),
                    7 => format!("{}{}{}", ["-", "+"][self.next(2)], self.ws(), self.expr(depth - 1)),
                    8 => {
                        const UNARY: &[&str] = &[
                            "sqrt", "exp", "ln", "abs", "sin", "cos", "tan", "asin", "acos",
                            "atan", "sinh", "cosh", "tanh", "asinh", "acosh", "atanh", "floor",
                            "ceil", "round", "signum",
                        ];
                        format!(
                            "{}{}({})",
                            UNARY[self.next(UNARY.len())],
                            self.ws(),
                            self.expr(depth - 1)
                        )
                    }
                    _ => match self.next(3) {
                        0 => format!(
                            "atan2({},{}{})",
                            self.expr(depth - 1),
                            self.ws(),
                            self.expr(depth - 1)
                        ),
                        which => {
                            let name = if which == 1 { "max" } else { "min" };
                            let n = 1 + self.next(4);
                            let args: Vec<String> =
                                (0..n).map(|_| self.expr(depth - 1)).collect();
                            format!("{name}({})", args.join(", "))
                        }
                    },
                }
            }
        }

        let mut g = Gen(0x0dd_ba11_5eed_0001);
        let mut failures = Vec::new();
        let mut finite = 0u32;
        for _ in 0..20_000 {
            let depth = 1 + g.next(5);
            let expr = g.expr(depth);
            let theirs = meval::eval_str(&expr)
                .unwrap_or_else(|e| panic!("generator produced {expr:?}, which meval rejects: {e}"));
            match eval(&expr) {
                Ok(ours) if same(theirs, ours) => {
                    if ours.is_finite() {
                        finite += 1;
                    }
                }
                other => {
                    if failures.len() < 20 {
                        failures.push(format!("{expr:?}: meval={theirs:?} ours={other:?}"));
                    }
                }
            }
        }
        println!("WELL_FORMED finite={finite} of 20000");
        assert!(failures.is_empty(), "disagreements:\n{}", failures.join("\n"));
        assert!(finite > 5_000, "too few finite results to mean much: {finite}");
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
