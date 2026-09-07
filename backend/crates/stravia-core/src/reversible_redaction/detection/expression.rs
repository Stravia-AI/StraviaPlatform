//! Compiler for the finite local-filter vocabulary in the pinned Betterleaks snapshot.
//! No validation expressions, dynamic functions, I/O, or runtime regex compilation.
use super::super::RedactionError;
use regex::bytes::{Regex, RegexBuilder};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
};

type Result<T, E = RedactionError> = std::result::Result<T, E>;
fn invalid() -> RedactionError {
    RedactionError::Detection
}

pub(super) fn regex(source: &str) -> Result<Regex> {
    // Go/RE2 uses Unicode code points for dot, repetition, and case folding,
    // but ASCII Perl classes and word boundaries. Rust's default Perl classes
    // are Unicode; translate only escape tokens (including inside classes).
    let mut pattern = String::with_capacity(source.len());
    let mut chars = source.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            // RE2 treats an opening brace without a counted repetition as
            // literal text; Rust requires it to be escaped.
            if ch == '{' && !counted_repetition(chars.clone()) {
                pattern.push('\\');
            }
            pattern.push(ch);
            continue;
        }
        let escaped = chars.next().ok_or_else(invalid)?;
        match escaped {
            'd' => pattern.push_str("[0-9]"),
            'D' => pattern.push_str("[^0-9]"),
            'w' => pattern.push_str("[A-Za-z0-9_]"),
            'W' => pattern.push_str("[^A-Za-z0-9_]"),
            's' => pattern.push_str("[ \\t\\n\\f\\r]"),
            'S' => pattern.push_str("[^ \\t\\n\\f\\r]"),
            'b' => pattern.push_str("(?-u:\\b)"),
            'B' => pattern.push_str("(?-u:\\B)"),
            _ => {
                pattern.push('\\');
                pattern.push(escaped);
            }
        }
    }
    RegexBuilder::new(&pattern)
        .size_limit(32 * 1024 * 1024)
        .build()
        .map_err(|_| invalid())
}

fn counted_repetition(mut chars: std::str::Chars<'_>) -> bool {
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_digit() {
        return false;
    }
    let mut separator = false;
    for ch in chars {
        match ch {
            '0'..='9' => {}
            ',' if !separator => separator = true,
            '}' => return true,
            _ => return false,
        }
    }
    false
}

#[derive(Clone, PartialEq)]
enum Token {
    Name(String),
    String(String),
    Number(f64),
    Symbol(String),
}

fn lex(source: &str) -> Result<Vec<Token>> {
    let bytes = source.as_bytes();
    let mut pos = 0;
    let mut result = Vec::new();
    while pos < bytes.len() {
        if bytes[pos].is_ascii_whitespace() {
            pos += 1;
            continue;
        }
        if bytes[pos..].starts_with(b"//") {
            while pos < bytes.len() && bytes[pos] != b'\n' {
                pos += 1;
            }
            continue;
        }
        let start = pos;
        match bytes[pos] {
            b'`' | b'"' => {
                let quote = bytes[pos];
                pos += 1;
                let content = pos;
                while pos < bytes.len() && bytes[pos] != quote {
                    if quote == b'"' && bytes[pos] == b'\\' {
                        pos += 1;
                    }
                    pos += 1;
                }
                if pos >= bytes.len() {
                    return Err(invalid());
                }
                let value = if quote == b'`' {
                    source[content..pos].to_owned()
                } else {
                    serde_json::from_str::<String>(&source[start..=pos]).map_err(|_| invalid())?
                };
                pos += 1;
                result.push(Token::String(value));
            }
            b'0'..=b'9' => {
                while pos < bytes.len() && (bytes[pos].is_ascii_digit() || bytes[pos] == b'.') {
                    pos += 1;
                }
                result.push(Token::Number(
                    source[start..pos].parse().map_err(|_| invalid())?,
                ));
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                while pos < bytes.len()
                    && (bytes[pos].is_ascii_alphanumeric() || matches!(bytes[pos], b'_' | b'.'))
                {
                    pos += 1;
                }
                result.push(Token::Name(source[start..pos].to_owned()));
            }
            _ => {
                let pair = bytes.get(pos..pos + 2);
                if matches!(pair, Some(b"&&" | b"||" | b"==" | b"!=" | b"<=" | b">=")) {
                    pos += 2;
                } else if b"()[],:;?+-!<>=".contains(&bytes[pos]) && bytes[pos].is_ascii() {
                    pos += 1;
                } else {
                    return Err(invalid());
                }
                result.push(Token::Symbol(source[start..pos].to_owned()));
            }
        }
    }
    Ok(result)
}

#[derive(Clone, Copy)]
enum Field {
    Secret,
    Match,
    Raw,
    Line,
    Start,
    End,
    LineStart,
    LineEnd,
    Path,
}
#[derive(Clone, Copy)]
enum Function {
    Entropy,
    TokenRatio,
    FailsTokenEfficiency,
    Confidence,
    Len,
    Min,
    Max,
    Split,
    Join,
}
enum Expr {
    String(String),
    Number(f64),
    Bool(bool),
    Variable(usize),
    Field(Field),
    Array(Vec<Expr>),
    Unary(Box<Expr>),
    Binary(String, Box<Expr>, Box<Expr>),
    Conditional(Box<Expr>, Box<Expr>, Box<Expr>),
    Slice(Box<Expr>, Option<Box<Expr>>, Option<Box<Expr>>),
    Index(Box<Expr>, Box<Expr>),
    Regex(Box<Expr>, Regex, bool),
    Contains(Box<Expr>, Vec<String>),
    Call(Function, Vec<Expr>),
}
pub(super) struct Program {
    bindings: Vec<Expr>,
    result: Expr,
}
struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    names: HashMap<String, usize>,
}
impl Parser {
    fn symbol(&mut self, symbol: &str) -> bool {
        if matches!(self.tokens.get(self.pos), Some(Token::Symbol(value)) if value == symbol) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn require(&mut self, symbol: &str) -> Result<()> {
        if self.symbol(symbol) {
            Ok(())
        } else {
            Err(invalid())
        }
    }
    fn next(&mut self) -> Result<Token> {
        let token = self.tokens.get(self.pos).cloned().ok_or_else(invalid)?;
        self.pos += 1;
        Ok(token)
    }
    fn expression(&mut self, minimum: u8) -> Result<Expr> {
        let mut left = if self.symbol("!") {
            Expr::Unary(Box::new(self.expression(7)?))
        } else if self.symbol("(") {
            let value = self.expression(0)?;
            self.require(")")?;
            value
        } else if self.symbol("[") {
            let mut values = Vec::new();
            while !self.symbol("]") {
                values.push(self.expression(0)?);
                if self.symbol("]") {
                    break;
                }
                self.require(",")?;
            }
            Expr::Array(values)
        } else {
            match self.next()? {
                Token::String(value) => Expr::String(value),
                Token::Number(value) => Expr::Number(value),
                Token::Name(name) if name == "true" || name == "false" => {
                    Expr::Bool(name == "true")
                }
                Token::Name(name) if name == "finding" || name == "attributes" => {
                    self.require("[")?;
                    let Token::String(key) = self.next()? else {
                        return Err(invalid());
                    };
                    self.require("]")?;
                    let field = match (name.as_str(), key.as_str()) {
                        ("attributes", "path") => Field::Path,
                        ("finding", "secret") => Field::Secret,
                        ("finding", "match") => Field::Match,
                        ("finding", "fragment_raw") => Field::Raw,
                        ("finding", "line") => Field::Line,
                        ("finding", "match_start_idx") => Field::Start,
                        ("finding", "match_end_idx") => Field::End,
                        ("finding", "match_line_start_idx") => Field::LineStart,
                        ("finding", "match_line_end_idx") => Field::LineEnd,
                        _ => return Err(invalid()),
                    };
                    Expr::Field(field)
                }
                Token::Name(name) if self.symbol("(") => {
                    let mut args = Vec::new();
                    while !self.symbol(")") {
                        args.push(self.expression(0)?);
                        if self.symbol(")") {
                            break;
                        }
                        self.require(",")?;
                    }
                    Self::call(name.strip_prefix("filter.").unwrap_or(&name), args)?
                }
                Token::Name(name) => Expr::Variable(*self.names.get(&name).ok_or_else(invalid)?),
                _ => return Err(invalid()),
            }
        };
        loop {
            if self.symbol("[") {
                let first = if self.symbol(":") {
                    None
                } else {
                    let value = self.expression(0)?;
                    if self.symbol("]") {
                        left = Expr::Index(Box::new(left), Box::new(value));
                        continue;
                    }
                    self.require(":")?;
                    Some(Box::new(value))
                };
                let end = if self.symbol("]") {
                    None
                } else {
                    let value = self.expression(0)?;
                    self.require("]")?;
                    Some(Box::new(value))
                };
                left = Expr::Slice(Box::new(left), first, end);
                continue;
            }
            let Some(Token::Symbol(op)) = self.tokens.get(self.pos) else {
                break;
            };
            let priority = match op.as_str() {
                "||" => 1,
                "&&" => 2,
                "==" | "!=" => 3,
                "<" | "<=" | ">" | ">=" => 4,
                "+" | "-" => 5,
                _ => break,
            };
            if priority < minimum {
                break;
            }
            let op = op.clone();
            self.pos += 1;
            left = Expr::Binary(op, Box::new(left), Box::new(self.expression(priority + 1)?));
        }
        if minimum == 0 && self.symbol("?") {
            let yes = self.expression(0)?;
            self.require(":")?;
            let no = self.expression(0)?;
            left = Expr::Conditional(Box::new(left), Box::new(yes), Box::new(no));
        }
        Ok(left)
    }
    fn call(name: &str, mut args: Vec<Expr>) -> Result<Expr> {
        if matches!(name, "matchesAny" | "findMatch" | "containsAny") {
            if args.len() != 2 {
                return Err(invalid());
            }
            let pattern = args.pop().ok_or_else(invalid)?;
            let input = Box::new(args.pop().ok_or_else(invalid)?);
            let patterns = match pattern {
                Expr::String(s) if name == "findMatch" => vec![s],
                Expr::Array(values) if name != "findMatch" => values
                    .into_iter()
                    .map(|v| match v {
                        Expr::String(s) => Ok(s),
                        _ => Err(invalid()),
                    })
                    .collect::<Result<Vec<_>>>()?,
                _ => return Err(invalid()),
            };
            if name == "containsAny" {
                return Ok(Expr::Contains(
                    input,
                    patterns.into_iter().map(|s| s.to_lowercase()).collect(),
                ));
            }
            let joined = patterns
                .iter()
                .map(|p| format!("(?:{p})"))
                .collect::<Vec<_>>()
                .join("|");
            if joined.is_empty() {
                return Err(invalid());
            }
            return Ok(Expr::Regex(input, regex(&joined)?, name == "findMatch"));
        }
        let (function, count) = match name {
            "entropy" => (Function::Entropy, 1),
            "tokenRatio" => (Function::TokenRatio, 1),
            "failsTokenEfficiency" => (Function::FailsTokenEfficiency, 1),
            "setConfidence" => (Function::Confidence, 1),
            "len" => (Function::Len, 1),
            "min" => (Function::Min, 2),
            "max" => (Function::Max, 2),
            "split" => (Function::Split, 2),
            "join" => (Function::Join, 2),
            _ => return Err(invalid()),
        };
        if args.len() != count {
            return Err(invalid());
        }
        Ok(Expr::Call(function, args))
    }
}

pub(super) struct Context<'a> {
    pub secret: &'a str,
    pub matched: &'a str,
    pub raw: &'a str,
    pub line: &'a str,
    pub start: usize,
    pub end: usize,
    pub line_start: usize,
    pub line_end: usize,
    pub tokenizer: &'a tiktoken_rs::CoreBPE,
    pub words: &'a HashSet<&'static str>,
    pub word_lengths: &'a [usize],
}
#[derive(Clone)]
enum Value<'a> {
    Bytes(Cow<'a, [u8]>),
    Number(f64),
    Bool(bool),
    Array(Vec<Value<'a>>),
}
impl<'a> Value<'a> {
    fn bytes(&self) -> Result<&[u8]> {
        if let Self::Bytes(v) = self {
            Ok(v)
        } else {
            Err(invalid())
        }
    }
    fn number(&self) -> Result<f64> {
        if let Self::Number(v) = self {
            Ok(*v)
        } else {
            Err(invalid())
        }
    }
    fn boolean(&self) -> Result<bool> {
        if let Self::Bool(v) = self {
            Ok(*v)
        } else {
            Err(invalid())
        }
    }
    fn length(&self) -> Result<usize> {
        match self {
            Self::Bytes(v) => Ok(v.len()),
            Self::Array(v) => Ok(v.len()),
            _ => Err(invalid()),
        }
    }
}
fn bytes(value: &str) -> Value<'_> {
    Value::Bytes(Cow::Borrowed(value.as_bytes()))
}
impl Program {
    pub(super) fn compile(source: &str) -> Result<Self> {
        if source.trim().is_empty() {
            return Ok(Self {
                bindings: Vec::new(),
                result: Expr::Bool(false),
            });
        }
        let mut parser = Parser {
            tokens: lex(source)?,
            pos: 0,
            names: HashMap::new(),
        };
        let mut bindings = Vec::new();
        while matches!(parser.tokens.get(parser.pos), Some(Token::Name(name)) if name == "let") {
            parser.pos += 1;
            let Token::Name(name) = parser.next()? else {
                return Err(invalid());
            };
            parser.require("=")?;
            bindings.push(parser.expression(0)?);
            parser.require(";")?;
            parser.names.insert(name, bindings.len() - 1);
        }
        let result = parser.expression(0)?;
        if parser.pos != parser.tokens.len() {
            return Err(invalid());
        }
        let mut types = Vec::with_capacity(bindings.len());
        for binding in &bindings {
            types.push(binding.value_type(&types)?);
        }
        if result.value_type(&types)? != ValueType::Bool {
            return Err(invalid());
        }
        Ok(Self { bindings, result })
    }
    pub(super) fn evaluate(&self, context: &Context<'_>) -> Result<bool> {
        let mut bindings = Vec::with_capacity(self.bindings.len());
        for expression in &self.bindings {
            bindings.push(expression.evaluate(context, &bindings)?);
        }
        self.result.evaluate(context, &bindings)?.boolean()
    }
}
#[derive(Clone, Copy, PartialEq)]
enum ValueType {
    Bytes,
    Number,
    Bool,
    Strings,
}
impl Expr {
    fn value_type(&self, bindings: &[ValueType]) -> Result<ValueType> {
        use ValueType::{Bool, Bytes, Number, Strings};
        let require = |actual, expected| {
            if actual == expected {
                Ok(())
            } else {
                Err(invalid())
            }
        };
        Ok(match self {
            Self::String(_) => Bytes,
            Self::Number(_) => Number,
            Self::Bool(_) => Bool,
            Self::Variable(index) => *bindings.get(*index).ok_or_else(invalid)?,
            Self::Field(field) => match field {
                Field::Start | Field::End | Field::LineStart | Field::LineEnd => Number,
                _ => Bytes,
            },
            Self::Array(values) => {
                for value in values {
                    require(value.value_type(bindings)?, Bytes)?;
                }
                Strings
            }
            Self::Unary(value) => {
                require(value.value_type(bindings)?, Bool)?;
                Bool
            }
            Self::Binary(op, left, right) => {
                let left = left.value_type(bindings)?;
                let right = right.value_type(bindings)?;
                require(left, right)?;
                match op.as_str() {
                    "||" | "&&" => {
                        require(left, Bool)?;
                        Bool
                    }
                    "+" if left == Bytes => Bytes,
                    "+" | "-" => {
                        require(left, Number)?;
                        Number
                    }
                    "<" | "<=" | ">" | ">=" => {
                        require(left, Number)?;
                        Bool
                    }
                    "==" | "!=" if left != Strings => Bool,
                    _ => return Err(invalid()),
                }
            }
            Self::Conditional(condition, yes, no) => {
                require(condition.value_type(bindings)?, Bool)?;
                let yes = yes.value_type(bindings)?;
                require(no.value_type(bindings)?, yes)?;
                yes
            }
            Self::Slice(value, start, end) => {
                let value = value.value_type(bindings)?;
                if !matches!(value, Bytes | Strings) {
                    return Err(invalid());
                }
                for bound in [start, end].into_iter().flatten() {
                    require(bound.value_type(bindings)?, Number)?;
                }
                value
            }
            Self::Index(value, index) => {
                require(value.value_type(bindings)?, Strings)?;
                require(index.value_type(bindings)?, Number)?;
                Bytes
            }
            Self::Regex(input, _, extract) => {
                require(input.value_type(bindings)?, Bytes)?;
                if *extract { Bytes } else { Bool }
            }
            Self::Contains(input, _) => {
                require(input.value_type(bindings)?, Bytes)?;
                Bool
            }
            Self::Call(function, args) => {
                let types = args
                    .iter()
                    .map(|arg| arg.value_type(bindings))
                    .collect::<Result<Vec<_>>>()?;
                match function {
                    Function::Entropy | Function::TokenRatio => {
                        require(types[0], Bytes)?;
                        Number
                    }
                    Function::FailsTokenEfficiency => {
                        require(types[0], Bytes)?;
                        Bool
                    }
                    Function::Confidence => {
                        require(types[0], Bytes)?;
                        Bytes
                    }
                    Function::Len => {
                        if !matches!(types[0], Bytes | Strings) {
                            return Err(invalid());
                        }
                        Number
                    }
                    Function::Min | Function::Max => {
                        require(types[0], Number)?;
                        require(types[1], Number)?;
                        Number
                    }
                    Function::Split => {
                        require(types[0], Bytes)?;
                        require(types[1], Bytes)?;
                        Strings
                    }
                    Function::Join => {
                        require(types[0], Strings)?;
                        require(types[1], Bytes)?;
                        Bytes
                    }
                }
            }
        })
    }
    fn evaluate<'a>(&'a self, context: &Context<'a>, bindings: &[Value<'a>]) -> Result<Value<'a>> {
        Ok(match self {
            Self::String(s) => bytes(s),
            Self::Number(v) => Value::Number(*v),
            Self::Bool(v) => Value::Bool(*v),
            Self::Variable(index) => bindings.get(*index).ok_or_else(invalid)?.clone(),
            Self::Field(field) => match field {
                Field::Secret => bytes(context.secret),
                Field::Match => bytes(context.matched),
                Field::Raw => bytes(context.raw),
                Field::Line => bytes(context.line),
                Field::Path => bytes(""),
                Field::Start => Value::Number(context.start as f64),
                Field::End => Value::Number(context.end as f64),
                Field::LineStart => Value::Number(context.line_start as f64),
                Field::LineEnd => Value::Number(context.line_end as f64),
            },
            Self::Array(values) => Value::Array(
                values
                    .iter()
                    .map(|v| v.evaluate(context, bindings))
                    .collect::<Result<_>>()?,
            ),
            Self::Unary(value) => Value::Bool(!value.evaluate(context, bindings)?.boolean()?),
            Self::Binary(op, left, right) => {
                let left = left.evaluate(context, bindings)?;
                if op == "||" && left.boolean()? {
                    return Ok(Value::Bool(true));
                }
                if op == "&&" && !left.boolean()? {
                    return Ok(Value::Bool(false));
                }
                let right = right.evaluate(context, bindings)?;
                match op.as_str() {
                    "||" | "&&" => Value::Bool(right.boolean()?),
                    "+" if matches!(left, Value::Bytes(_)) => {
                        let mut out = left.bytes()?.to_vec();
                        out.extend_from_slice(right.bytes()?);
                        Value::Bytes(Cow::Owned(out))
                    }
                    "+" => Value::Number(left.number()? + right.number()?),
                    "-" => Value::Number(left.number()? - right.number()?),
                    "<" => Value::Bool(left.number()? < right.number()?),
                    "<=" => Value::Bool(left.number()? <= right.number()?),
                    ">" => Value::Bool(left.number()? > right.number()?),
                    ">=" => Value::Bool(left.number()? >= right.number()?),
                    "==" | "!=" => {
                        let equal = match (&left, &right) {
                            (Value::Number(a), Value::Number(b)) => a == b,
                            (Value::Bytes(a), Value::Bytes(b)) => a == b,
                            (Value::Bool(a), Value::Bool(b)) => a == b,
                            _ => return Err(invalid()),
                        };
                        Value::Bool(equal == (op == "=="))
                    }
                    _ => return Err(invalid()),
                }
            }
            Self::Conditional(condition, yes, no) => {
                if condition.evaluate(context, bindings)?.boolean()? {
                    yes.evaluate(context, bindings)?
                } else {
                    no.evaluate(context, bindings)?
                }
            }
            Self::Slice(value, start, end) => {
                let value = value.evaluate(context, bindings)?;
                let start = if let Some(start) = start {
                    index(start.evaluate(context, bindings)?.number()?)?
                } else {
                    0
                };
                let end = if let Some(end) = end {
                    index(end.evaluate(context, bindings)?.number()?)?
                } else {
                    value.length()?
                };
                if start > end || end > value.length()? {
                    return Err(invalid());
                }
                match value {
                    Value::Bytes(Cow::Borrowed(v)) => Value::Bytes(Cow::Borrowed(&v[start..end])),
                    Value::Bytes(Cow::Owned(v)) => Value::Bytes(Cow::Owned(v[start..end].to_vec())),
                    Value::Array(v) => Value::Array(v[start..end].to_vec()),
                    _ => return Err(invalid()),
                }
            }
            Self::Index(value, at) => {
                let at = index(at.evaluate(context, bindings)?.number()?)?;
                let Value::Array(values) = value.evaluate(context, bindings)? else {
                    return Err(invalid());
                };
                values.get(at).ok_or_else(invalid)?.clone()
            }
            Self::Regex(input, pattern, extract) => {
                let input = input.evaluate(context, bindings)?;
                if *extract {
                    Value::Bytes(Cow::Owned(
                        pattern
                            .find(input.bytes()?)
                            .map_or(&[][..], |m| m.as_bytes())
                            .to_vec(),
                    ))
                } else {
                    Value::Bool(pattern.is_match(input.bytes()?))
                }
            }
            Self::Contains(input, terms) => {
                let input = input.evaluate(context, bindings)?;
                let lowered = String::from_utf8_lossy(input.bytes()?).to_lowercase();
                Value::Bool(terms.iter().any(|term| lowered.contains(term)))
            }
            Self::Call(function, arguments) => {
                let args = arguments
                    .iter()
                    .map(|a| a.evaluate(context, bindings))
                    .collect::<Result<Vec<_>>>()?;
                match function {
                    Function::Entropy => {
                        let input = args[0].bytes()?;
                        let mut frequencies = [0usize; 256];
                        for b in input {
                            frequencies[*b as usize] += 1;
                        }
                        Value::Number(
                            frequencies
                                .iter()
                                .filter(|n| **n > 0)
                                .map(|n| {
                                    let p = *n as f64 / input.len() as f64;
                                    -p * p.log2()
                                })
                                .sum(),
                        )
                    }
                    Function::TokenRatio | Function::FailsTokenEfficiency => {
                        let input = std::str::from_utf8(args[0].bytes()?).map_err(|_| invalid())?;
                        let analyzed = if input.len() < 20 && input.contains(['\r', '\n']) {
                            Cow::Owned(input.replace(['\r', '\n'], ""))
                        } else {
                            Cow::Borrowed(input)
                        };
                        let count = context.tokenizer.encode_ordinary(&analyzed).len();
                        let ratio = if count == 0 {
                            0.0
                        } else {
                            analyzed.len() as f64 / count as f64
                        };
                        if matches!(function, Function::TokenRatio) {
                            Value::Number(ratio)
                        } else {
                            let lowered = analyzed.to_lowercase();
                            let has_word = |minimum: usize| {
                                (0..lowered.len()).any(|start| {
                                    context
                                        .word_lengths
                                        .iter()
                                        .copied()
                                        .skip_while(|length| *length < minimum)
                                        .take_while(|length| *length <= lowered.len() - start)
                                        .any(|length| {
                                            lowered
                                                .get(start..start + length)
                                                .is_some_and(|s| context.words.contains(s))
                                        })
                                })
                            };
                            Value::Bool(
                                count > 0
                                    && (ratio >= 2.5
                                        || has_word(5)
                                        || analyzed.len() < 12 && ratio >= 2.1 && has_word(4)),
                            )
                        }
                    }
                    Function::Confidence => {
                        if !matches!(args[0].bytes()?, b"low" | b"medium" | b"high") {
                            return Err(invalid());
                        }
                        args[0].clone()
                    }
                    Function::Len => Value::Number(args[0].length()? as f64),
                    Function::Min => Value::Number(args[0].number()?.min(args[1].number()?)),
                    Function::Max => Value::Number(args[0].number()?.max(args[1].number()?)),
                    Function::Split => {
                        let input = args[0].bytes()?;
                        let separator = args[1].bytes()?;
                        if separator.len() != 1 {
                            return Err(invalid());
                        }
                        Value::Array(
                            input
                                .split(|b| *b == separator[0])
                                .map(|v| Value::Bytes(Cow::Owned(v.to_vec())))
                                .collect(),
                        )
                    }
                    Function::Join => {
                        let Value::Array(values) = &args[0] else {
                            return Err(invalid());
                        };
                        let separator = args[1].bytes()?;
                        let mut out = Vec::new();
                        for (i, value) in values.iter().enumerate() {
                            if i > 0 {
                                out.extend_from_slice(separator);
                            }
                            out.extend_from_slice(value.bytes()?);
                        }
                        Value::Bytes(Cow::Owned(out))
                    }
                }
            }
        })
    }
}
fn index(value: f64) -> Result<usize> {
    if !value.is_finite() || value < 0.0 || value.fract() != 0.0 || value > usize::MAX as f64 {
        return Err(invalid());
    }
    Ok(value as usize)
}
