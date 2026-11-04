use std::collections::HashMap;

pub const MAX_PARSE_DEPTH: usize = 64;

#[derive(Debug, Clone, Default)]
pub struct IfContext {
    pub success: bool,
    pub failure: bool,
    pub cancelled: bool,
    pub results: HashMap<String, String>,
    pub needs_outputs: HashMap<String, HashMap<String, String>>,
    pub inputs: HashMap<String, String>,
    pub matrix: HashMap<String, String>,
    pub env: HashMap<String, String>,
    pub gitforest: HashMap<String, String>,
    pub steps: HashMap<String, HashMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq)]
enum Val {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
}

impl Val {
    fn truthy(&self) -> bool {
        match self {
            Val::Null => false,
            Val::Bool(b) => *b,
            Val::Num(n) => *n != 0.0,
            Val::Str(s) => !s.is_empty(),
        }
    }

    fn stringify(&self) -> String {
        match self {
            Val::Null => String::new(),
            Val::Bool(b) => b.to_string(),
            Val::Num(n) => {
                if n.fract() == 0.0 && n.abs() < 1e15 {
                    format!("{}", *n as i64)
                } else {
                    format!("{n}")
                }
            }
            Val::Str(s) => s.clone(),
        }
    }

    fn as_num(&self) -> Option<f64> {
        match self {
            Val::Num(n) => Some(*n),
            Val::Str(s) => s.trim().parse::<f64>().ok(),
            Val::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Val::Null => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Deref(Vec<String>),
    Call(String, Vec<Expr>),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Eq(Box<Expr>, Box<Expr>),
    Ne(Box<Expr>, Box<Expr>),
    Lt(Box<Expr>, Box<Expr>),
    Gt(Box<Expr>, Box<Expr>),
    Le(Box<Expr>, Box<Expr>),
    Ge(Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Num(f64),
    Dot,
    LParen,
    RParen,
    Comma,
    Bang,
    And,
    Or,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

fn tokenize(input: &str) -> Option<Vec<Tok>> {
    let chars: Vec<char> = input.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '(' => {
                toks.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                toks.push(Tok::RParen);
                i += 1;
            }
            ',' => {
                toks.push(Tok::Comma);
                i += 1;
            }
            '.' => {
                toks.push(Tok::Dot);
                i += 1;
            }
            '&' if chars.get(i + 1) == Some(&'&') => {
                toks.push(Tok::And);
                i += 2;
            }
            '|' if chars.get(i + 1) == Some(&'|') => {
                toks.push(Tok::Or);
                i += 2;
            }
            '=' if chars.get(i + 1) == Some(&'=') => {
                toks.push(Tok::Eq);
                i += 2;
            }
            '!' if chars.get(i + 1) == Some(&'=') => {
                toks.push(Tok::Ne);
                i += 2;
            }
            '!' => {
                toks.push(Tok::Bang);
                i += 1;
            }
            '<' if chars.get(i + 1) == Some(&'=') => {
                toks.push(Tok::Le);
                i += 2;
            }
            '>' if chars.get(i + 1) == Some(&'=') => {
                toks.push(Tok::Ge);
                i += 2;
            }
            '<' => {
                toks.push(Tok::Lt);
                i += 1;
            }
            '>' => {
                toks.push(Tok::Gt);
                i += 1;
            }
            '\'' | '"' => {
                let quote = c;
                i += 1;
                let mut s = String::new();
                let mut closed = false;
                while i < chars.len() {
                    let d = chars[i];
                    if d == '\\' && i + 1 < chars.len() {
                        let e = chars[i + 1];
                        match e {
                            'n' => s.push('\n'),
                            't' => s.push('\t'),
                            other => s.push(other),
                        }
                        i += 2;
                        continue;
                    }
                    if d == quote {
                        if quote == '\'' && i + 1 < chars.len() && chars[i + 1] == '\'' {
                            s.push('\'');
                            i += 2;
                            continue;
                        }
                        closed = true;
                        i += 1;
                        break;
                    }
                    s.push(d);
                    i += 1;
                }
                if !closed {
                    return None;
                }
                toks.push(Tok::Str(s));
            }
            d if d.is_ascii_digit() => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                let raw: String = chars[start..i].iter().collect();
                let num: f64 = raw.parse().ok()?;
                toks.push(Tok::Num(num));
            }
            d if d.is_ascii_alphabetic() || d == '_' => {
                let start = i;
                while i < chars.len()
                    && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == '-')
                {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                toks.push(Tok::Ident(name));
            }
            _ => return None,
        }
    }
    Some(toks)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn bump(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn guard(&mut self) -> bool {
        self.depth += 1;
        self.depth <= MAX_PARSE_DEPTH
    }

    fn parse_or(&mut self) -> Option<Expr> {
        if !self.guard() {
            return None;
        }
        let mut left = self.parse_and()?;
        while self.peek() == Some(&Tok::Or) {
            self.bump();
            let right = self.parse_and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        self.depth -= 1;
        Some(left)
    }

    fn parse_and(&mut self) -> Option<Expr> {
        if !self.guard() {
            return None;
        }
        let mut left = self.parse_equality()?;
        while self.peek() == Some(&Tok::And) {
            self.bump();
            let right = self.parse_equality()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        self.depth -= 1;
        Some(left)
    }

    fn parse_equality(&mut self) -> Option<Expr> {
        if !self.guard() {
            return None;
        }
        let mut left = self.parse_relational()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Eq) => true,
                Some(Tok::Ne) => false,
                _ => break,
            };
            self.bump();
            let right = self.parse_relational()?;
            left = if op {
                Expr::Eq(Box::new(left), Box::new(right))
            } else {
                Expr::Ne(Box::new(left), Box::new(right))
            };
        }
        self.depth -= 1;
        Some(left)
    }

    fn parse_relational(&mut self) -> Option<Expr> {
        if !self.guard() {
            return None;
        }
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Lt) => 0,
                Some(Tok::Gt) => 1,
                Some(Tok::Le) => 2,
                Some(Tok::Ge) => 3,
                _ => break,
            };
            self.bump();
            let right = self.parse_unary()?;
            left = match op {
                0 => Expr::Lt(Box::new(left), Box::new(right)),
                1 => Expr::Gt(Box::new(left), Box::new(right)),
                2 => Expr::Le(Box::new(left), Box::new(right)),
                _ => Expr::Ge(Box::new(left), Box::new(right)),
            };
        }
        self.depth -= 1;
        Some(left)
    }

    fn parse_unary(&mut self) -> Option<Expr> {
        if !self.guard() {
            return None;
        }
        let out = if self.peek() == Some(&Tok::Bang) {
            self.bump();
            self.parse_unary().map(|e| Expr::Not(Box::new(e)))
        } else {
            self.parse_primary()
        };
        self.depth -= 1;
        out
    }

    fn parse_primary(&mut self) -> Option<Expr> {
        if !self.guard() {
            return None;
        }
        let out = match self.bump()? {
            Tok::LParen => {
                let e = self.parse_or()?;
                if self.bump()? != Tok::RParen {
                    return None;
                }
                Some(e)
            }
            Tok::Str(s) => Some(Expr::Str(s)),
            Tok::Num(n) => Some(Expr::Num(n)),
            Tok::Ident(name) => {
                if name == "true" {
                    return Some(Expr::Bool(true));
                }
                if name == "false" {
                    return Some(Expr::Bool(false));
                }
                if name == "null" {
                    return Some(Expr::Null);
                }
                if self.peek() == Some(&Tok::LParen) {
                    self.bump();
                    let mut args = Vec::new();
                    if self.peek() != Some(&Tok::RParen) {
                        loop {
                            args.push(self.parse_or()?);
                            if self.peek() == Some(&Tok::Comma) {
                                self.bump();
                            } else {
                                break;
                            }
                        }
                    }
                    if self.bump()? != Tok::RParen {
                        return None;
                    }
                    Some(Expr::Call(name, args))
                } else {
                    let mut parts = vec![name];
                    while self.peek() == Some(&Tok::Dot) {
                        self.bump();
                        match self.bump()? {
                            Tok::Ident(p) => parts.push(p),
                            _ => return None,
                        }
                    }
                    Some(Expr::Deref(parts))
                }
            }
            _ => None,
        };
        self.depth -= 1;
        out
    }
}

fn parse_expr(input: &str) -> Option<Expr> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let toks = tokenize(trimmed)?;
    if toks.is_empty() {
        return None;
    }
    let mut parser = Parser {
        toks,
        pos: 0,
        depth: 0,
    };
    let expr = parser.parse_or()?;
    if parser.pos != parser.toks.len() {
        return None;
    }
    Some(expr)
}

fn unwrap_gitforest_expression(input: &str) -> &str {
    let trimmed = input.trim();
    if let Some(inner) = trimmed
        .strip_prefix("${{")
        .and_then(|s| s.strip_suffix("}}"))
    {
        inner.trim()
    } else {
        trimmed
    }
}

fn resolve_deref(parts: &[String], ctx: &IfContext) -> Val {
    let get = |map: &HashMap<String, String>, key: &str| -> Val {
        map.get(key).cloned().map(Val::Str).unwrap_or(Val::Null)
    };
    match parts.first().map(String::as_str) {
        Some("needs") if parts.len() == 3 && parts[2] == "result" => get(&ctx.results, &parts[1]),
        Some("needs") if parts.len() == 4 && parts[2] == "outputs" => ctx
            .needs_outputs
            .get(&parts[1])
            .and_then(|m| m.get(&parts[3]))
            .cloned()
            .map(Val::Str)
            .unwrap_or(Val::Null),
        Some("inputs") if parts.len() == 2 => get(&ctx.inputs, &parts[1]),
        Some("matrix") if parts.len() == 2 => get(&ctx.matrix, &parts[1]),
        Some("env") if parts.len() == 2 => get(&ctx.env, &parts[1]),
        Some("gitforest") if parts.len() == 2 => get(&ctx.gitforest, &parts[1]),
        Some("steps") if parts.len() == 3 && parts[2] == "outcome" => ctx
            .steps
            .get(&parts[1])
            .and_then(|m| m.get("outcome"))
            .cloned()
            .map(Val::Str)
            .unwrap_or(Val::Null),
        Some("steps") if parts.len() == 4 && parts[2] == "outputs" => ctx
            .steps
            .get(&parts[1])
            .and_then(|m| m.get(&parts[3]))
            .cloned()
            .map(Val::Str)
            .unwrap_or(Val::Null),
        _ => Val::Null,
    }
}

fn eval_call(name: &str, args: &[Expr], ctx: &IfContext) -> Val {
    match name {
        "success" => Val::Bool(ctx.success),
        "failure" => Val::Bool(ctx.failure),
        "always" => Val::Bool(true),
        "cancelled" => Val::Bool(ctx.cancelled),
        "contains" | "startsWith" | "endsWith" if args.len() == 2 => {
            let hay = eval_node(&args[0], ctx).stringify().to_lowercase();
            let needle = eval_node(&args[1], ctx).stringify().to_lowercase();
            let hit = match name {
                "contains" => hay.contains(&needle),
                _ if needle.is_empty() => true,
                "startsWith" => hay.starts_with(&needle),
                _ => hay.ends_with(&needle),
            };
            Val::Bool(hit)
        }
        _ => Val::Null,
    }
}

fn eval_node(expr: &Expr, ctx: &IfContext) -> Val {
    match expr {
        Expr::Null => Val::Null,
        Expr::Bool(b) => Val::Bool(*b),
        Expr::Num(n) => Val::Num(*n),
        Expr::Str(s) => Val::Str(s.clone()),
        Expr::Deref(parts) => resolve_deref(parts, ctx),
        Expr::Call(name, args) => eval_call(name, args, ctx),
        Expr::Not(e) => Val::Bool(!eval_node(e, ctx).truthy()),
        Expr::And(a, b) => Val::Bool(eval_node(a, ctx).truthy() && eval_node(b, ctx).truthy()),
        Expr::Or(a, b) => Val::Bool(eval_node(a, ctx).truthy() || eval_node(b, ctx).truthy()),
        Expr::Eq(a, b) => {
            let va = eval_node(a, ctx);
            let vb = eval_node(b, ctx);
            Val::Bool(match (&va, &vb) {
                (Val::Num(x), Val::Num(y)) => x == y,
                _ => va.stringify().to_lowercase() == vb.stringify().to_lowercase(),
            })
        }
        Expr::Ne(a, b) => {
            let va = eval_node(a, ctx);
            let vb = eval_node(b, ctx);
            Val::Bool(match (&va, &vb) {
                (Val::Num(x), Val::Num(y)) => x != y,
                _ => va.stringify().to_lowercase() != vb.stringify().to_lowercase(),
            })
        }
        Expr::Lt(a, b) => compare(a, b, ctx, |o| o.is_lt()),
        Expr::Gt(a, b) => compare(a, b, ctx, |o| o.is_gt()),
        Expr::Le(a, b) => compare(a, b, ctx, |o| o.is_le()),
        Expr::Ge(a, b) => compare(a, b, ctx, |o| o.is_ge()),
    }
}

fn compare(a: &Expr, b: &Expr, ctx: &IfContext, pick: impl Fn(std::cmp::Ordering) -> bool) -> Val {
    let va = eval_node(a, ctx);
    let vb = eval_node(b, ctx);
    if let (Some(x), Some(y)) = (va.as_num(), vb.as_num()) {
        return Val::Bool(pick(x.total_cmp(&y)));
    }
    Val::Bool(pick(va.stringify().cmp(&vb.stringify())))
}

pub fn is_valid(input: &str) -> bool {
    let inner = unwrap_gitforest_expression(input);
    !inner.is_empty() && parse_expr(inner).is_some()
}

pub fn mentions_cancelled(input: &str) -> bool {
    let inner = unwrap_gitforest_expression(input);
    parse_expr(inner).is_some_and(|e| mentions_call(&e, "cancelled"))
}

fn mentions_call(expr: &Expr, name: &str) -> bool {
    match expr {
        Expr::Call(n, args) => n == name || args.iter().any(|a| mentions_call(a, name)),
        Expr::Not(e) => mentions_call(e, name),
        Expr::And(a, b)
        | Expr::Or(a, b)
        | Expr::Eq(a, b)
        | Expr::Ne(a, b)
        | Expr::Lt(a, b)
        | Expr::Gt(a, b)
        | Expr::Le(a, b)
        | Expr::Ge(a, b) => mentions_call(a, name) || mentions_call(b, name),
        _ => false,
    }
}

pub fn eval(input: &str, ctx: &IfContext) -> bool {
    let inner = unwrap_gitforest_expression(input);
    if inner.is_empty() {
        return false;
    }
    parse_expr(inner)
        .map(|e| eval_node(&e, ctx).truthy())
        .unwrap_or(false)
}

fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn deref_root_name(parts: &[String]) -> Option<&str> {
    match parts.first().map(String::as_str) {
        // secrets.* never substitutes anywhere: conditions must not leak secrets.
        Some("needs" | "inputs" | "matrix" | "env" | "gitforest" | "steps") => {
            parts.first().map(String::as_str)
        }
        _ => None,
    }
}

pub fn substitute_known(input: &str, ctx: &IfContext) -> String {
    substitute_roots(
        input,
        ctx,
        &["needs", "inputs", "matrix", "env", "gitforest"],
    )
}

pub fn substitute_all(input: &str, ctx: &IfContext) -> String {
    substitute_roots_quoted(
        input,
        ctx,
        &["needs", "inputs", "matrix", "env", "gitforest", "steps"],
        true,
    )
}

pub fn substitute_all_raw(input: &str, ctx: &IfContext) -> String {
    substitute_roots_quoted(
        input,
        ctx,
        &["needs", "inputs", "matrix", "env", "gitforest", "steps"],
        false,
    )
}

pub fn substitute_roots(input: &str, ctx: &IfContext, roots: &[&str]) -> String {
    substitute_roots_quoted(input, ctx, roots, true)
}

fn substitute_roots_quoted(input: &str, ctx: &IfContext, roots: &[&str], quote: bool) -> String {
    let mut out = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' && chars.get(i + 1) == Some(&'{') && chars.get(i + 2) == Some(&'{') {
            if let Some(end) = find_wrapped_end(&chars, i + 3) {
                let inner: String = chars[i + 3..end].iter().collect();
                let trimmed = inner.trim();
                if let Some(quoted) = substitute_wrapped(trimmed, ctx, roots, quote) {
                    out.push_str(&quoted);
                    i = end + 2;
                    continue;
                }
                out.push_str("${{");
                out.push_str(&inner);
                out.push_str("}}");
                i = end + 2;
                continue;
            }
        }
        if chars[i].is_ascii_alphabetic() || chars[i] == '_' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == '-')
            {
                i += 1;
            }
            let mut parts = vec![chars[start..i].iter().collect::<String>()];
            let mut j = i;
            while chars.get(j) == Some(&'.')
                && chars
                    .get(j + 1)
                    .is_some_and(|c| c.is_ascii_alphabetic() || *c == '_')
            {
                j += 1;
                let seg_start = j;
                while j < chars.len()
                    && (chars[j].is_ascii_alphanumeric() || chars[j] == '_' || chars[j] == '-')
                {
                    j += 1;
                }
                parts.push(chars[seg_start..j].iter().collect());
            }
            let is_call = chars.get(j) == Some(&'(');
            if !is_call
                && parts.len() > 1
                && deref_root_name(&parts).is_some_and(|r| roots.contains(&r))
            {
                let value = resolve_deref(&parts, ctx).stringify();
                if quote {
                    out.push_str(&quote_literal(&value));
                } else {
                    out.push_str(&value);
                }
                i = j;
                continue;
            }
            out.push_str(&chars[start..i].iter().collect::<String>());
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn substitute_wrapped(
    trimmed: &str,
    ctx: &IfContext,
    roots: &[&str],
    quote: bool,
) -> Option<String> {
    let expr = parse_expr(trimmed)?;
    let Expr::Deref(parts) = &expr else {
        return None;
    };
    if !deref_root_name(parts).is_some_and(|r| roots.contains(&r)) {
        return None;
    }
    let value = resolve_deref(parts, ctx).stringify();
    Some(if quote { quote_literal(&value) } else { value })
}

fn find_wrapped_end(chars: &[char], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 1 < chars.len() {
        if chars[i] == '}' && chars[i + 1] == '}' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn simplify(expr: Expr) -> Expr {
    match expr {
        Expr::Not(e) => match simplify(*e) {
            Expr::Bool(b) => Expr::Bool(!b),
            Expr::Not(inner) => *inner,
            e => Expr::Not(Box::new(e)),
        },
        Expr::And(a, b) => match (simplify(*a), simplify(*b)) {
            (Expr::Bool(false), _) | (_, Expr::Bool(false)) => Expr::Bool(false),
            (Expr::Bool(true), e) | (e, Expr::Bool(true)) => e,
            (a, b) => Expr::And(Box::new(a), Box::new(b)),
        },
        Expr::Or(a, b) => match (simplify(*a), simplify(*b)) {
            (Expr::Bool(true), _) | (_, Expr::Bool(true)) => Expr::Bool(true),
            (Expr::Bool(false), e) | (e, Expr::Bool(false)) => e,
            (a, b) => Expr::Or(Box::new(a), Box::new(b)),
        },
        Expr::Call(name, args) => {
            let args: Vec<Expr> = args.into_iter().map(simplify).collect();
            Expr::Call(name, args)
        }
        other => other,
    }
}

fn serialize(expr: &Expr) -> String {
    match expr {
        Expr::Null => "null".to_string(),
        Expr::Bool(b) => b.to_string(),
        Expr::Num(n) => Val::Num(*n).stringify(),
        Expr::Str(s) => quote_literal(s),
        Expr::Deref(parts) => parts.join("."),
        Expr::Call(name, args) => {
            let inner: Vec<String> = args.iter().map(serialize).collect();
            format!("{name}({})", inner.join(", "))
        }
        Expr::Not(e) => format!("!{}", serialize_atom(e)),
        Expr::And(a, b) => format!("{} && {}", serialize_atom(a), serialize_atom(b)),
        Expr::Or(a, b) => format!("{} || {}", serialize_atom(a), serialize_atom(b)),
        Expr::Eq(a, b) => format!("{} == {}", serialize_atom(a), serialize_atom(b)),
        Expr::Ne(a, b) => format!("{} != {}", serialize_atom(a), serialize_atom(b)),
        Expr::Lt(a, b) => format!("{} < {}", serialize_atom(a), serialize_atom(b)),
        Expr::Gt(a, b) => format!("{} > {}", serialize_atom(a), serialize_atom(b)),
        Expr::Le(a, b) => format!("{} <= {}", serialize_atom(a), serialize_atom(b)),
        Expr::Ge(a, b) => format!("{} >= {}", serialize_atom(a), serialize_atom(b)),
    }
}

fn serialize_atom(expr: &Expr) -> String {
    match expr {
        Expr::And(..) | Expr::Or(..) => format!("({})", serialize(expr)),
        _ => serialize(expr),
    }
}

pub fn fold_constants(input: &str) -> String {
    let inner = unwrap_gitforest_expression(input);
    let Some(expr) = parse_expr(inner) else {
        return "false".to_string();
    };
    let empty = IfContext::default();
    let folded = fold_with_context(expr, &empty);
    serialize(&simplify(folded))
}

fn fold_with_context(expr: Expr, ctx: &IfContext) -> Expr {
    match expr {
        Expr::Deref(parts) => match resolve_deref(&parts, ctx) {
            Val::Null => Expr::Deref(parts),
            Val::Bool(b) => Expr::Bool(b),
            Val::Num(n) => Expr::Num(n),
            Val::Str(s) => Expr::Str(s),
        },
        Expr::Not(e) => match fold_with_context(*e, ctx) {
            Expr::Bool(b) => Expr::Bool(!b),
            e => Expr::Not(Box::new(e)),
        },
        Expr::And(a, b) => match (fold_with_context(*a, ctx), fold_with_context(*b, ctx)) {
            (Expr::Bool(x), Expr::Bool(y)) => Expr::Bool(x && y),
            (a, b) => Expr::And(Box::new(a), Box::new(b)),
        },
        Expr::Or(a, b) => match (fold_with_context(*a, ctx), fold_with_context(*b, ctx)) {
            (Expr::Bool(x), Expr::Bool(y)) => Expr::Bool(x || y),
            (a, b) => Expr::Or(Box::new(a), Box::new(b)),
        },
        Expr::Eq(a, b) => fold_compare(*a, *b, ctx, true),
        Expr::Ne(a, b) => fold_compare(*a, *b, ctx, false),
        Expr::Call(name, args) => Expr::Call(
            name,
            args.into_iter()
                .map(|e| fold_with_context(e, ctx))
                .collect(),
        ),
        other => other,
    }
}

fn fold_compare(a: Expr, b: Expr, ctx: &IfContext, is_eq: bool) -> Expr {
    let fa = fold_with_context(a, ctx);
    let fb = fold_with_context(b, ctx);
    let result = match (&fa, &fb) {
        (Expr::Str(x), Expr::Str(y)) => Some(x.to_lowercase() == y.to_lowercase()),
        (Expr::Bool(x), Expr::Bool(y)) => Some(x == y),
        (Expr::Num(x), Expr::Num(y)) => Some(x == y),
        (Expr::Null, Expr::Null) => Some(true),
        (Expr::Null, Expr::Str(y)) => Some(y.is_empty()),
        (Expr::Str(x), Expr::Null) => Some(x.is_empty()),
        _ => None,
    };
    match result {
        Some(eq) => Expr::Bool(if is_eq { eq } else { !eq }),
        None => {
            if is_eq {
                Expr::Eq(Box::new(fa), Box::new(fb))
            } else {
                Expr::Ne(Box::new(fa), Box::new(fb))
            }
        }
    }
}

pub fn sanitize_step_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn mentions_steps(input: &str) -> bool {
    let inner = unwrap_gitforest_expression(input);
    parse_expr(inner).is_some_and(|e| mentions_deref_root(&e, "steps"))
}

pub fn steps_refs(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(expr) = parse_expr(unwrap_gitforest_expression(input)) {
        collect_steps_refs(&expr, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

fn mentions_deref_root(expr: &Expr, root: &str) -> bool {
    match expr {
        Expr::Deref(parts) => parts.first().is_some_and(|r| r == root),
        Expr::Call(_, args) => args.iter().any(|a| mentions_deref_root(a, root)),
        Expr::Not(e) => mentions_deref_root(e, root),
        Expr::And(a, b)
        | Expr::Or(a, b)
        | Expr::Eq(a, b)
        | Expr::Ne(a, b)
        | Expr::Lt(a, b)
        | Expr::Gt(a, b)
        | Expr::Le(a, b)
        | Expr::Ge(a, b) => mentions_deref_root(a, root) || mentions_deref_root(b, root),
        _ => false,
    }
}

fn collect_steps_refs(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Deref(parts) => {
            if parts.first().is_some_and(|r| r == "steps") && parts.len() > 1 {
                out.push(parts[1].clone());
            }
        }
        Expr::Call(_, args) => {
            for arg in args {
                collect_steps_refs(arg, out);
            }
        }
        Expr::Not(e) => collect_steps_refs(e, out),
        Expr::And(a, b)
        | Expr::Or(a, b)
        | Expr::Eq(a, b)
        | Expr::Ne(a, b)
        | Expr::Lt(a, b)
        | Expr::Gt(a, b)
        | Expr::Le(a, b)
        | Expr::Ge(a, b) => {
            collect_steps_refs(a, out);
            collect_steps_refs(b, out);
        }
        _ => {}
    }
}

pub fn compile_shell_guard(input: &str) -> Option<String> {
    let inner = unwrap_gitforest_expression(input);
    let expr = simplify(parse_expr(inner)?);
    compile_shell_node(&expr)
}

fn compile_shell_node(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Bool(true) => Some("true".to_string()),
        Expr::Bool(false) => Some("false".to_string()),
        Expr::Null => Some("false".to_string()),
        Expr::Num(n) => Some((*n != 0.0).to_string()),
        Expr::Call(name, args) if args.is_empty() => match name.as_str() {
            "success" => Some("[ \"$GF_FAILED\" -eq 0 ]".to_string()),
            "failure" => Some("[ \"$GF_FAILED\" -ne 0 ]".to_string()),
            "always" => Some("true".to_string()),
            "cancelled" => Some("false".to_string()),
            _ => None,
        },
        Expr::Not(e) => compile_shell_node(e).map(|s| format!("! ( {s} )")),
        Expr::And(a, b) => Some(format!(
            "{} && {}",
            compile_shell_branch(a)?,
            compile_shell_branch(b)?
        )),
        Expr::Or(a, b) => Some(format!(
            "{} || {}",
            compile_shell_branch(a)?,
            compile_shell_branch(b)?
        )),
        Expr::Eq(a, b) => compile_shell_eq(a, b, false),
        Expr::Ne(a, b) => compile_shell_eq(a, b, true),
        Expr::Deref(_) | Expr::Str(_) => compile_shell_operand(expr).map(|o| format!("[ -n {o} ]")),
        _ => None,
    }
}

fn compile_shell_eq(a: &Expr, b: &Expr, neg: bool) -> Option<String> {
    let x = compile_shell_operand(a)?;
    let y = compile_shell_operand(b)?;
    let op = if neg { "!=" } else { "=" };
    Some(format!(
        "[ \"$(printf '%s' {x} | tr '[:upper:]' '[:lower:]')\" {op} \"$(printf '%s' {y} | tr '[:upper:]' '[:lower:]')\" ]"
    ))
}

fn compile_shell_operand(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Str(s) => Some(shell_single_quote(s)),
        Expr::Num(n) => Some(Val::Num(*n).stringify()),
        Expr::Bool(b) => Some(b.to_string()),
        Expr::Deref(parts) => compile_steps_lookup(parts),
        _ => None,
    }
}

fn compile_steps_lookup(parts: &[String]) -> Option<String> {
    if parts.first().map(String::as_str) != Some("steps") || parts.len() < 3 {
        return None;
    }
    if parts.len() == 3 && parts[2] == "outcome" {
        let var = format!("GF_STEP_{}_OUTCOME", sanitize_step_id(&parts[1]));
        return Some(format!("\"${{{var}:-}}\""));
    }
    if parts.len() == 4 && parts[2] == "outputs" {
        return Some(format!(
            "\"$(gf_step_output {} {})\"",
            shell_single_quote(&parts[1]),
            shell_single_quote(&parts[3])
        ));
    }
    None
}

fn compile_shell_branch(expr: &Expr) -> Option<String> {
    let inner = compile_shell_node(expr)?;
    match expr {
        Expr::And(..) | Expr::Or(..) => Some(format!("( {inner} )")),
        _ => Some(inner),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> IfContext {
        IfContext {
            success: true,
            failure: false,
            cancelled: false,
            results: HashMap::from([
                ("build".to_string(), "success".to_string()),
                ("flaky".to_string(), "failure".to_string()),
            ]),
            needs_outputs: HashMap::from([(
                "build".to_string(),
                HashMap::from([("sha".to_string(), "abc123".to_string())]),
            )]),
            inputs: HashMap::from([("env".to_string(), "prod".to_string())]),
            matrix: HashMap::from([("os".to_string(), "linux".to_string())]),
            env: HashMap::from([("MODE".to_string(), "fast".to_string())]),
            gitforest: HashMap::from([("event_name".to_string(), "push".to_string())]),
            steps: HashMap::new(),
        }
    }

    #[test]
    fn literals_and_status_functions() {
        let c = ctx();
        assert!(eval("true", &c));
        assert!(!eval("false", &c));
        assert!(eval("${{ success() }}", &c));
        assert!(!eval("failure()", &c));
        assert!(eval("always()", &c));
        assert!(!eval("cancelled()", &c));
    }

    #[test]
    fn comparisons_and_logic() {
        let c = ctx();
        assert!(eval("needs.build.result == 'success'", &c));
        assert!(eval("needs.flaky.result != 'success'", &c));
        assert!(eval("needs.build.outputs.sha == \"abc123\"", &c));
        assert!(eval("inputs.env == 'prod' && matrix.os == 'linux'", &c));
        assert!(eval("env.MODE == 'fast' || env.MODE == 'slow'", &c));
        assert!(!eval("!(inputs.env == 'prod')", &c));
        assert!(eval("(false || true) && always()", &c));
    }

    #[test]
    fn gitforest_case_insensitive_equality() {
        let c = ctx();
        assert!(eval("inputs.env == 'PROD'", &c));
        assert!(eval("'AbC' == 'aBc'", &c));
        assert!(!eval("'AbC' != 'aBc'", &c));
    }

    #[test]
    fn helpers_and_numbers() {
        let c = ctx();
        assert!(eval("contains(inputs.env, 'ro')", &c));
        assert!(eval("startsWith(matrix.os, 'lin')", &c));
        assert!(eval("endsWith(env.MODE, 'ast')", &c));
        assert!(eval("1 < 2 && 3 >= 3 && 4 > 5 == false", &c));
    }

    #[test]
    fn unknown_and_malformed_are_falsy() {
        let c = ctx();
        assert!(!eval("needs.ghost.result == 'success'", &c));
        assert!(!eval("bogus_function(1, 2)", &c));
        assert!(!eval("inputs.env ===", &c));
        assert!(!eval("", &c));
        assert!(!eval("   ", &c));
        assert!(!eval("(((((", &c));
    }

    #[test]
    fn steps_refs_compile_to_runtime_lookups() {
        let guard = compile_shell_guard("steps.build.outcome == 'success'").unwrap();
        assert!(guard.contains("GF_STEP_BUILD_OUTCOME"), "{guard}");
        let guard = compile_shell_guard("steps.build.outputs.sha == 'abc123'").unwrap();
        assert!(guard.contains("gf_step_output"), "{guard}");
        assert!(guard.contains("tr "), "{guard}");
        assert!(mentions_steps("failure() && steps.a.outcome == 'success'"));
        assert!(!mentions_steps("failure() && inputs.x == 'y'"));
        assert_eq!(sanitize_step_id("build-x_1"), "BUILD_X_1");
        assert!(compile_shell_guard("steps.a.outcome < 'b'").is_none());
        assert!(compile_shell_guard("needs.build.result == 'success'").is_none());
        assert_eq!(
            steps_refs("steps.a.outcome == 'x' || steps.b.outputs.k == 'y'"),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn substitution_and_fold() {
        let c = ctx();
        let sub = substitute_known("inputs.env == 'prod' && failure()", &c);
        assert_eq!(sub, "'prod' == 'prod' && failure()");
        assert_eq!(fold_constants("true && false || true"), "true");
        assert_eq!(fold_constants("'a' == 'a'"), "true");
        let guard = compile_shell_guard("failure() && true").unwrap();
        assert!(guard.contains("GF_FAILED"));
    }
}
