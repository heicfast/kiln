//! Evaluation of `cfg(...)` expressions for target-specific dependency tables.
//!
//! Supports everything a `[target.'cfg(...)'.dependencies]` row needs against
//! the compile target: `target_os`, `target_arch`, `target_family`,
//! `target_env`, `target_endian`, `target_pointer_width`, `target_vendor`,
//! `unix`, `windows`, plus `all`, `any` and `not`, nested to any depth.

/// The facts about a compile target that cfg expressions see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetFacts {
    /// `target_arch`, e.g. `x86_64`.
    pub arch: String,
    /// `target_os`, e.g. `linux`.
    pub os: String,
    /// `target_family`: `unix`, `windows`, or absent.
    pub family: Option<String>,
    /// `target_env`: `gnu`, `musl`, `msvc`, or absent.
    pub env: Option<String>,
    /// `target_endian`: `little` or `big`.
    pub endian: &'static str,
    /// `target_pointer_width`: `32` or `64`.
    pub pointer_width: &'static str,
    /// `target_vendor`: `unknown`, `apple`, `pc`, ...
    pub vendor: String,
}

impl TargetFacts {
    /// Derive the facts from a target triple the way rustc does: the triple's
    /// fields, with the family implied by the os.
    pub fn from_triple(triple: &str) -> Self {
        let parts: Vec<&str> = triple.split('-').collect();
        let arch = parts.first().copied().unwrap_or("unknown").to_owned();
        let last = parts.last().copied().unwrap_or("unknown");
        // Triples are arch-vendor-os-env, with the env sometimes absent.
        let (vendor, os, env) = match parts.len() {
            4 => (parts[1].to_owned(), parts[2].to_owned(), Some(parts[3].to_owned())),
            3 => (parts[1].to_owned(), parts[2].to_owned(), None),
            2 => ("unknown".to_owned(), last.to_owned(), None),
            _ => ("unknown".to_owned(), "unknown".to_owned(), None),
        };
        // The triple says `darwin`; the cfg the compiler exposes says `macos`.
        let os = if os == "darwin" { "macos".to_owned() } else { os };
        let family = match os.as_str() {
            "linux" | "macos" | "ios" | "freebsd" | "netbsd" | "openbsd" | "android" | "solaris"
            | "dragonfly" | "illumos" | "haiku" | "watchos" | "tvos" => Some("unix".to_owned()),
            "windows" => Some("windows".to_owned()),
            _ => None,
        };
        TargetFacts {
            arch: arch.clone(),
            os,
            family,
            env: env.filter(|e| !e.is_empty()),
            endian: if arch.contains("be") { "big" } else { "little" },
            pointer_width: if arch.contains("64") { "64" } else { "32" },
            vendor,
        }
    }

    /// Evaluate a bare cfg word against the target.
    fn word(&self, word: &str) -> bool {
        match word {
            "unix" => self.family.as_deref() == Some("unix"),
            "windows" => self.family.as_deref() == Some("windows"),
            _ => false,
        }
    }

    /// Evaluate a `key = "value"` predicate against the target.
    fn pair(&self, key: &str, value: &str) -> kiln_core::Result<bool> {
        Ok(match key {
            "target_os" => value == self.os,
            "target_arch" => value == self.arch,
            "target_family" => self.family.as_deref() == Some(value),
            "target_env" => self.env.as_deref() == Some(value),
            "target_endian" => value == self.endian,
            "target_pointer_width" => value == self.pointer_width,
            "target_vendor" => value == self.vendor,
            // Only the x86_64 baseline that rustc's own target cfg lists
            // carry; anything else is a target feature no baseline target has.
            "target_feature" => matches!(value, "crt-static" | "fxsr" | "sse" | "sse2") && self.arch == "x86_64",
            other => {
                return Err(kiln_core::Error::unsupported(format!(
                    "cfg({other} = ...): kiln does not evaluate this cfg key yet"
                )))
            }
        })
    }
}

/// A parsed cfg expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CfgExpr {
    /// `cfg(unix)` and friends.
    Word(String),
    /// `cfg(target_os = "linux")`.
    Equal(String, String),
    /// `cfg(not(...))`.
    Not(Box<CfgExpr>),
    /// `cfg(all(...))`.
    All(Vec<CfgExpr>),
    /// `cfg(any(...))`.
    Any(Vec<CfgExpr>),
}

/// Evaluate a cfg expression string like `cfg(unix)`.
pub fn eval_cfg(expr: &str, facts: &TargetFacts) -> kiln_core::Result<bool> {
    parse_cfg(expr)?.eval(facts)
}

impl CfgExpr {
    /// Evaluate against target facts.
    pub fn eval(&self, facts: &TargetFacts) -> kiln_core::Result<bool> {
        match self {
            CfgExpr::Word(w) => Ok(facts.word(w)),
            CfgExpr::Equal(k, v) => facts.pair(k, v),
            CfgExpr::Not(inner) => Ok(!inner.eval(facts)?),
            CfgExpr::All(list) => {
                for item in list {
                    if !item.eval(facts)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            CfgExpr::Any(list) => {
                for item in list {
                    if item.eval(facts)? {
                        return Ok(true);
                    }
                }
                Ok(list.is_empty())
            }
        }
    }
}

/// Parse `cfg(...)` into an expression tree.
pub fn parse_cfg(expr: &str) -> kiln_core::Result<CfgExpr> {
    let expr = expr.trim();
    let inner = expr
        .strip_prefix("cfg(")
        .and_then(|e| e.strip_suffix(')'))
        .ok_or_else(|| kiln_core::Error::parse("cfg expression", format!("`{expr}` is not a cfg(...) call")))?;
    let mut parser = Parser { tokens: tokenize(inner)?, pos: 0, src: inner };
    let expr = parser.expr()?;
    if parser.pos != parser.tokens.len() {
        return Err(kiln_core::Error::parse(
            "cfg expression",
            format!("trailing tokens after expression in `{inner}`"),
        ));
    }
    Ok(expr)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    Str(String),
    Eq,
    Comma,
    Open,
    Close,
}

fn tokenize(src: &str) -> kiln_core::Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut chars = src.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            ' ' | '\t' | '\n' => {
                chars.next();
            }
            ',' => {
                chars.next();
                tokens.push(Token::Comma);
            }
            '=' => {
                chars.next();
                tokens.push(Token::Eq);
            }
            '(' => {
                chars.next();
                tokens.push(Token::Open);
            }
            ')' => {
                chars.next();
                tokens.push(Token::Close);
            }
            '"' => {
                chars.next();
                let mut s = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '"' {
                        closed = true;
                        break;
                    }
                    s.push(c);
                }
                if !closed {
                    return Err(kiln_core::Error::parse("cfg expression", "unterminated string"));
                }
                tokens.push(Token::Str(s));
            }
            c if c.is_ascii_alphanumeric() || c == '_' => {
                let mut w = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_alphanumeric() || c == '_' {
                        w.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                tokens.push(Token::Word(w));
            }
            other => {
                return Err(kiln_core::Error::parse(
                    "cfg expression",
                    format!("unexpected character `{other}` in `{src}`"),
                ))
            }
        }
    }
    Ok(tokens)
}

struct Parser<'a> {
    tokens: Vec<Token>,
    pos: usize,
    src: &'a str,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    /// A comma-separated list, allowing one trailing separator position.
    fn list(&mut self) -> kiln_core::Result<Vec<CfgExpr>> {
        let mut items = Vec::new();
        if matches!(self.peek(), Some(Token::Close) | None) {
            return Ok(items);
        }
        loop {
            items.push(self.expr()?);
            match self.peek() {
                Some(Token::Comma) => {
                    self.next();
                    if matches!(self.peek(), Some(Token::Close) | None) {
                        break;
                    }
                }
                _ => break,
            }
        }
        Ok(items)
    }

    fn expr(&mut self) -> kiln_core::Result<CfgExpr> {
        match self.next() {
            Some(Token::Word(w)) => match w.as_str() {
                "all" | "any" | "not" => {
                    if self.next() != Some(Token::Open) {
                        return Err(self.err(format!("`{w}` needs a parenthesised list")));
                    }
                    let items = self.list()?;
                    if self.next() != Some(Token::Close) {
                        return Err(self.err(format!("`{w}(...)` is missing its closing paren")));
                    }
                    Ok(match w.as_str() {
                        "all" => CfgExpr::All(items),
                        "any" => CfgExpr::Any(items),
                        _ if items.len() == 1 => CfgExpr::Not(Box::new(items.into_iter().next().unwrap())),
                        _ => return Err(self.err("`not` takes exactly one operand".to_owned())),
                    })
                }
                _ => {
                    // A bare word, or the left side of `key = "value"`.
                    if self.peek() == Some(&Token::Eq) {
                        self.next();
                        match self.next() {
                            Some(Token::Str(v)) => Ok(CfgExpr::Equal(w, v)),
                            _ => Err(self.err(format!("cfg({w} = ...) needs a quoted string"))),
                        }
                    } else {
                        Ok(CfgExpr::Word(w))
                    }
                }
            },
            _ => Err(self.err("expected a predicate".to_owned())),
        }
    }

    fn err(&self, what: String) -> kiln_core::Error {
        kiln_core::Error::parse("cfg expression", format!("{what} in `{}`", self.src))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linux() -> TargetFacts {
        TargetFacts::from_triple("x86_64-unknown-linux-gnu")
    }

    #[test]
    fn triples_derive_facts() {
        let f = linux();
        assert_eq!(f.arch, "x86_64");
        assert_eq!(f.os, "linux");
        assert_eq!(f.family.as_deref(), Some("unix"));
        assert_eq!(f.env.as_deref(), Some("gnu"));
        let mac = TargetFacts::from_triple("aarch64-apple-darwin");
        assert_eq!(mac.os, "macos"); // the triple says darwin; target_os says macos
        assert_eq!(mac.family.as_deref(), Some("unix"));
        assert_eq!(mac.env, None);
        let win = TargetFacts::from_triple("x86_64-pc-windows-msvc");
        assert_eq!(win.family.as_deref(), Some("windows"));
        assert_eq!(win.env.as_deref(), Some("msvc"));
        assert_eq!(win.vendor, "pc");
    }

    #[test]
    fn cfg_words_and_pairs() {
        assert!(eval_cfg("cfg(unix)", &linux()).unwrap());
        assert!(!eval_cfg("cfg(windows)", &linux()).unwrap());
        assert!(eval_cfg("cfg(target_os = \"linux\")", &linux()).unwrap());
        assert!(!eval_cfg("cfg(target_os = \"macos\")", &linux()).unwrap());
        assert!(eval_cfg("cfg(target_arch = \"x86_64\")", &linux()).unwrap());
        assert!(eval_cfg("cfg(target_pointer_width = \"64\")", &linux()).unwrap());
    }

    #[test]
    fn cfg_combinators_nest() {
        assert!(eval_cfg("cfg(all(unix, target_arch = \"x86_64\"))", &linux()).unwrap());
        assert!(!eval_cfg("cfg(all(unix, target_os = \"macos\"))", &linux()).unwrap());
        assert!(eval_cfg("cfg(any(windows, unix))", &linux()).unwrap());
        assert!(eval_cfg("cfg(not(windows))", &linux()).unwrap());
        assert!(!eval_cfg("cfg(not(unix))", &linux()).unwrap());
        assert!(eval_cfg("cfg(all(not(windows), any(target_env = \"gnu\", target_env = \"musl\")))", &linux()).unwrap());
        assert!(eval_cfg("cfg(all(all(unix)))", &linux()).unwrap());
        // `all()` with no operands is vacuously true, as rustc's cfg says.
        assert!(eval_cfg("cfg(all())", &linux()).unwrap());
    }

    #[test]
    fn malformed_cfgs_error() {
        assert!(eval_cfg("unix", &linux()).is_err());
        assert!(eval_cfg("cfg(target_os =)", &linux()).is_err());
        assert!(eval_cfg("cfg(,)", &linux()).is_err());
        assert!(eval_cfg("cfg(not())", &linux()).is_err());
        assert!(eval_cfg("cfg(all(unix,)", &linux()).is_err());
        assert!(eval_cfg("cfg(mystery_key = \"x\")", &linux()).is_err());
    }
}
