//! XPath 1.0 over the document tree.
//!
//! # Why this is written here rather than taken
//!
//! Every driver that speaks WebDriver names elements by XPath at least some of
//! the time — Selenium's own tutorials teach it before they teach selectors — so
//! a browser that answers the protocol and not this locator is a browser half of
//! the existing scripts cannot drive.
//!
//! There was no crate to take. The Rust XPath implementations each own their
//! tree: one is written against `sxd-document`, another against a DOM of its own,
//! and the mature one is a binding to a C library. All three would mean
//! converting the whole document into somebody else's node type on every query —
//! which is a copy of the page per call, and a second tree to keep true to the
//! first. So this walks ours.
//!
//! # What is here, and what is not
//!
//! The language a locator is actually written in: the thirteen axes, the four
//! node tests, predicates, the comparison and arithmetic operators, unions, and
//! the string, number and boolean functions from section 4. What is left out is
//! what a locator never contains — variable references, which have nothing to
//! bind to over a protocol that does not send bindings; namespace axes and
//! prefixes, because an HTML document has one namespace that matters and
//! `local-name()` is how a locator asks about the others.
//!
//! Anything unsupported is a parse error naming itself, never a silent empty
//! node-set: an empty answer reads as *nothing matched*, which would send whoever
//! wrote the expression looking at their page instead of at their XPath.
//!
//! # Attributes are nodes
//!
//! XPath's data model has an attribute node with a parent and a string-value, and
//! our DOM does not — an attribute here is a pair on the element. So a node-set
//! holds [`Item`], which is either a node or an attribute *of* a node. It costs
//! one enum and it is what makes `//a/@href` and `//div[@id="x"]` both work
//! without inventing node ids for things the tree never allocated.

use std::collections::HashMap;

use crate::node::{NodeData, NodeId};
use crate::tree::Document;

/// What went wrong with an expression, in a sentence naming the place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XPathError {
    /// What is wrong.
    pub message: String,
    /// How many bytes into the expression it was noticed.
    pub at: usize,
}

impl std::fmt::Display for XPathError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "{} (at character {})", self.message, self.at)
    }
}

impl std::error::Error for XPathError {}

/// One member of a node-set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    /// A node of the tree.
    Node(NodeId),
    /// An attribute of one, which the tree has no node for.
    Attribute {
        /// The element it is on, which is its parent in XPath's model.
        owner: NodeId,
        /// Its name.
        name: String,
        /// Its value, which is also its string-value.
        value: String,
    },
}

impl Item {
    /// The node this is, or the one it belongs to.
    fn anchor(&self) -> NodeId {
        match self {
            Self::Node(node) => *node,
            Self::Attribute { owner, .. } => *owner,
        }
    }
}

/// What an expression evaluates to. XPath 1.0 has exactly these four.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// A set of nodes, kept in document order and without duplicates.
    Nodes(Vec<Item>),
    /// A string.
    Text(String),
    /// A number, which XPath has only one of and which is a float.
    Number(f64),
    /// A boolean.
    Boolean(bool),
}

/// Evaluate `expression` against `document`, starting at its root.
///
/// The entry point a locator uses: it answers the nodes an expression selects, in
/// document order. An expression that is not a node-set — `count(//a)`, say — is
/// a mistake in a *locator* rather than in the expression, and is refused as one.
pub fn select(document: &Document, expression: &str) -> Result<Vec<NodeId>, XPathError> {
    select_from(document, document.root(), expression)
}

/// The same, with the expression evaluated relative to `context`.
pub fn select_from(
    document: &Document,
    context: NodeId,
    expression: &str,
) -> Result<Vec<NodeId>, XPathError> {
    match evaluate(document, context, expression)? {
        Value::Nodes(items) => Ok(items
            .into_iter()
            .filter_map(|item| match item {
                Item::Node(node) => Some(node),
                // An attribute is a node to XPath and is not one to a caller
                // holding node handles, so `//@href` selects nothing rather than
                // selecting the elements the attributes were on — which would be
                // answering a different question.
                Item::Attribute { .. } => None,
            })
            .collect()),
        other => Err(XPathError {
            message: format!(
                "this expression is {}, and a locator has to be a node-set",
                match other {
                    Value::Text(_) => "a string",
                    Value::Number(_) => "a number",
                    Value::Boolean(_) => "a boolean",
                    Value::Nodes(_) => unreachable!(),
                }
            ),
            at: 0,
        }),
    }
}

/// Evaluate `expression` and hand back its string-value.
///
/// What a caller wants when the expression is not a locator: `string(//a/@href)`,
/// `count(//li)`, `//h1/text()`. The four kinds convert to a string by the
/// specification's rules — a node-set by the string-value of its first node in
/// document order, a number without the trailing zero a formatter would add.
pub fn evaluate_to_string(
    document: &Document,
    context: NodeId,
    expression: &str,
) -> Result<String, XPathError> {
    let value = evaluate(document, context, expression)?;
    Ok(Engine::new(document).string(&value))
}

/// Evaluate `expression` and hand back whatever kind of value it is.
pub fn evaluate(
    document: &Document,
    context: NodeId,
    expression: &str,
) -> Result<Value, XPathError> {
    let parsed = parse(expression)?;
    let engine = Engine::new(document);
    engine.eval(
        &parsed,
        &Context {
            item: Item::Node(context),
            position: 1,
            size: 1,
        },
    )
}

// --- the expression, as parsed ---------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Expr {
    Or(Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Compare(Compare, Box<Expr>, Box<Expr>),
    Arith(Arith, Box<Expr>, Box<Expr>),
    Negate(Box<Expr>),
    Union(Box<Expr>, Box<Expr>),
    /// A location path, optionally rooted at what another expression selected.
    Path {
        /// Whether it starts at the document root.
        absolute: bool,
        /// What it starts from, for `foo(...)/bar` and `(a|b)/c`.
        start: Option<Box<Expr>>,
        steps: Vec<Step>,
    },
    Literal(String),
    Number(f64),
    Call(String, Vec<Expr>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Compare {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arith {
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
}

#[derive(Clone, Debug, PartialEq)]
struct Step {
    axis: Axis,
    test: Test,
    predicates: Vec<Expr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Axis {
    Child,
    Descendant,
    DescendantOrSelf,
    Parent,
    Ancestor,
    AncestorOrSelf,
    FollowingSibling,
    PrecedingSibling,
    Following,
    Preceding,
    Itself,
    Attribute,
}

impl Axis {
    /// Whether the axis runs backwards through the document.
    ///
    /// It decides what `position()` counts from, which is the one thing about the
    /// axes that is easy to get wrong: `preceding-sibling::li[1]` is the sibling
    /// *nearest* the context node, not the first one in the document.
    fn reverse(self) -> bool {
        matches!(
            self,
            Self::Parent
                | Self::Ancestor
                | Self::AncestorOrSelf
                | Self::PrecedingSibling
                | Self::Preceding
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Test {
    /// A name, matched case-insensitively because HTML lower-cases its tags and
    /// a locator is written the way the page's author wrote them.
    Name(String),
    /// `*`, which is every element — or every attribute, on that axis.
    Any,
    Text,
    Comment,
    /// `node()`, which is anything at all.
    Anything,
    ProcessingInstruction,
}

// --- lexing ----------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Name(String),
    Literal(String),
    Number(f64),
    /// An operator or punctuation, as it is written.
    Symbol(&'static str),
}

/// Turn an expression into tokens.
///
/// # The one hard rule
///
/// XPath is ambiguous without context: `*` is both *every element* and
/// *multiply*, and `div` is both an axis-less step and *divide*. The
/// specification resolves it by what came before — a `*` or a name like `and`,
/// `or`, `div`, `mod` is an operator exactly when the previous token is not `@`,
/// `::`, `(`, `[`, `,` or another operator. That test is why lexing carries state
/// rather than being a map over characters.
fn lex(input: &str) -> Result<Vec<(Token, usize)>, XPathError> {
    let bytes: Vec<char> = input.chars().collect();
    let mut tokens: Vec<(Token, usize)> = Vec::new();
    let mut at = 0;

    // What decides whether a name or a `*` is an operator.
    let operator_expected = |tokens: &Vec<(Token, usize)>| match tokens.last() {
        None => false,
        Some((Token::Name(_) | Token::Number(_) | Token::Literal(_), _)) => true,
        Some((Token::Symbol(symbol), _)) => matches!(*symbol, ")" | "]"),
    };

    while at < bytes.len() {
        let start = at;
        let character = bytes[at];

        if character.is_whitespace() {
            at += 1;
            continue;
        }

        // A string literal, in either quote.
        if character == '\'' || character == '"' {
            at += 1;
            let mut text = String::new();
            loop {
                let Some(&next) = bytes.get(at) else {
                    return Err(XPathError {
                        message: "a string literal was never closed".to_owned(),
                        at: start,
                    });
                };
                at += 1;
                if next == character {
                    break;
                }
                text.push(next);
            }
            tokens.push((Token::Literal(text), start));
            continue;
        }

        // A number. A leading `.` is a number only when a digit follows it;
        // otherwise it is the self step.
        if character.is_ascii_digit()
            || (character == '.' && bytes.get(at + 1).is_some_and(char::is_ascii_digit))
        {
            let mut text = String::new();
            while let Some(&next) = bytes.get(at) {
                if next.is_ascii_digit() || next == '.' {
                    text.push(next);
                    at += 1;
                } else {
                    break;
                }
            }
            let number = text.parse::<f64>().map_err(|_| XPathError {
                message: format!("{text:?} is not a number"),
                at: start,
            })?;
            tokens.push((Token::Number(number), start));
            continue;
        }

        // A name. Hyphens and dots are name characters in XPath, which is why
        // `descendant-or-self` and `normalize-space` lex as one token.
        if character.is_alphabetic() || character == '_' {
            let mut text = String::new();
            while let Some(&next) = bytes.get(at) {
                if next.is_alphanumeric() || matches!(next, '_' | '-' | '.') {
                    text.push(next);
                    at += 1;
                } else {
                    break;
                }
            }
            // A prefixed name — `svg:rect` — keeps only its local part: this
            // engine has no namespace bindings to resolve a prefix against, and
            // dropping it matches what a locator against an HTML page means.
            if bytes.get(at) == Some(&':') && bytes.get(at + 1) != Some(&':') {
                at += 1;
                let mut local = String::new();
                while let Some(&next) = bytes.get(at) {
                    if next.is_alphanumeric() || matches!(next, '_' | '-' | '.') {
                        local.push(next);
                        at += 1;
                    } else {
                        break;
                    }
                }
                if !local.is_empty() {
                    text = local;
                }
            }

            let is_operator =
                matches!(text.as_str(), "and" | "or" | "div" | "mod") && operator_expected(&tokens);
            if is_operator {
                let symbol = match text.as_str() {
                    "and" => "and",
                    "or" => "or",
                    "div" => "div",
                    _ => "mod",
                };
                tokens.push((Token::Symbol(symbol), start));
            } else {
                tokens.push((Token::Name(text), start));
            }
            continue;
        }

        // Punctuation, longest first.
        let two: String = bytes[at..].iter().take(2).collect();
        let symbol: Option<&'static str> = match two.as_str() {
            "//" => Some("//"),
            "::" => Some("::"),
            "!=" => Some("!="),
            "<=" => Some("<="),
            ">=" => Some(">="),
            ".." => Some(".."),
            _ => None,
        };
        if let Some(symbol) = symbol {
            at += 2;
            tokens.push((Token::Symbol(symbol), start));
            continue;
        }

        let symbol: &'static str = match character {
            '/' => "/",
            '(' => "(",
            ')' => ")",
            '[' => "[",
            ']' => "]",
            '@' => "@",
            ',' => ",",
            '.' => ".",
            '|' => "|",
            '+' => "+",
            '-' => "-",
            '=' => "=",
            '<' => "<",
            '>' => ">",
            '*' => {
                if operator_expected(&tokens) {
                    "*mul"
                } else {
                    "*"
                }
            }
            '$' => {
                return Err(XPathError {
                    message: "variable references are not supported: nothing binds them here"
                        .to_owned(),
                    at: start,
                });
            }
            other => {
                return Err(XPathError {
                    message: format!("{other:?} has no meaning in an expression"),
                    at: start,
                });
            }
        };
        at += 1;
        tokens.push((Token::Symbol(symbol), start));
    }

    Ok(tokens)
}

// --- parsing ---------------------------------------------------------------

struct Parser {
    tokens: Vec<(Token, usize)>,
    at: usize,
    /// Where the expression ended, for an error about what is missing.
    end: usize,
}

fn parse(input: &str) -> Result<Expr, XPathError> {
    let tokens = lex(input)?;
    if tokens.is_empty() {
        return Err(XPathError {
            message: "an empty expression selects nothing and is probably a mistake".to_owned(),
            at: 0,
        });
    }
    let mut parser = Parser {
        tokens,
        at: 0,
        end: input.len(),
    };
    let expression = parser.expr()?;
    if parser.at < parser.tokens.len() {
        let (token, at) = parser.tokens[parser.at].clone();
        return Err(XPathError {
            message: format!("{token:?} is left over after the expression"),
            at,
        });
    }
    Ok(expression)
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at).map(|(token, _)| token)
    }

    fn where_we_are(&self) -> usize {
        self.tokens.get(self.at).map_or(self.end, |(_, at)| *at)
    }

    fn eat(&mut self, symbol: &str) -> bool {
        if matches!(self.peek(), Some(Token::Symbol(found)) if *found == symbol) {
            self.at += 1;
            return true;
        }
        false
    }

    fn expect(&mut self, symbol: &str) -> Result<(), XPathError> {
        if self.eat(symbol) {
            return Ok(());
        }
        Err(XPathError {
            message: format!("{symbol:?} was expected here"),
            at: self.where_we_are(),
        })
    }

    fn expr(&mut self) -> Result<Expr, XPathError> {
        self.or()
    }

    fn or(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.and()?;
        while self.eat("or") {
            left = Expr::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.equality()?;
        while self.eat("and") {
            left = Expr::And(Box::new(left), Box::new(self.equality()?));
        }
        Ok(left)
    }

    fn equality(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.relational()?;
        loop {
            let operator = if self.eat("=") {
                Compare::Eq
            } else if self.eat("!=") {
                Compare::Ne
            } else {
                return Ok(left);
            };
            left = Expr::Compare(operator, Box::new(left), Box::new(self.relational()?));
        }
    }

    fn relational(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.additive()?;
        loop {
            let operator = if self.eat("<=") {
                Compare::Le
            } else if self.eat(">=") {
                Compare::Ge
            } else if self.eat("<") {
                Compare::Lt
            } else if self.eat(">") {
                Compare::Gt
            } else {
                return Ok(left);
            };
            left = Expr::Compare(operator, Box::new(left), Box::new(self.additive()?));
        }
    }

    fn additive(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.multiplicative()?;
        loop {
            let operator = if self.eat("+") {
                Arith::Add
            } else if self.eat("-") {
                Arith::Subtract
            } else {
                return Ok(left);
            };
            left = Expr::Arith(operator, Box::new(left), Box::new(self.multiplicative()?));
        }
    }

    fn multiplicative(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.unary()?;
        loop {
            let operator = if self.eat("*mul") {
                Arith::Multiply
            } else if self.eat("div") {
                Arith::Divide
            } else if self.eat("mod") {
                Arith::Modulo
            } else {
                return Ok(left);
            };
            left = Expr::Arith(operator, Box::new(left), Box::new(self.unary()?));
        }
    }

    fn unary(&mut self) -> Result<Expr, XPathError> {
        if self.eat("-") {
            return Ok(Expr::Negate(Box::new(self.unary()?)));
        }
        self.union()
    }

    fn union(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.path()?;
        while self.eat("|") {
            left = Expr::Union(Box::new(left), Box::new(self.path()?));
        }
        Ok(left)
    }

    /// A location path, or a primary expression that a path may continue from.
    fn path(&mut self) -> Result<Expr, XPathError> {
        // `//x` is `/descendant-or-self::node()/x`, which is the whole of what
        // the abbreviation means.
        if self.eat("//") {
            let mut steps = vec![Step {
                axis: Axis::DescendantOrSelf,
                test: Test::Anything,
                predicates: Vec::new(),
            }];
            steps.extend(self.steps()?);
            return Ok(Expr::Path {
                absolute: true,
                start: None,
                steps,
            });
        }
        if self.eat("/") {
            // A lone `/` is the root itself, which is a path with no steps.
            let steps = if self.starts_a_step() {
                self.steps()?
            } else {
                Vec::new()
            };
            return Ok(Expr::Path {
                absolute: true,
                start: None,
                steps,
            });
        }

        // A primary expression — a literal, a number, a function call or a
        // parenthesised expression — may be followed by more of a path.
        if let Some(primary) = self.primary()? {
            let mut steps = Vec::new();
            loop {
                if self.eat("//") {
                    steps.push(Step {
                        axis: Axis::DescendantOrSelf,
                        test: Test::Anything,
                        predicates: Vec::new(),
                    });
                } else if !self.eat("/") {
                    break;
                }
                steps.push(self.step()?);
            }
            if steps.is_empty() {
                return Ok(primary);
            }
            return Ok(Expr::Path {
                absolute: false,
                start: Some(Box::new(primary)),
                steps,
            });
        }

        Ok(Expr::Path {
            absolute: false,
            start: None,
            steps: self.steps()?,
        })
    }

    /// Whether what comes next could begin a step.
    fn starts_a_step(&self) -> bool {
        matches!(
            self.peek(),
            Some(Token::Name(_)) | Some(Token::Symbol("@" | "*" | "." | ".."))
        )
    }

    fn steps(&mut self) -> Result<Vec<Step>, XPathError> {
        let mut steps = vec![self.step()?];
        loop {
            if self.eat("//") {
                steps.push(Step {
                    axis: Axis::DescendantOrSelf,
                    test: Test::Anything,
                    predicates: Vec::new(),
                });
            } else if !self.eat("/") {
                return Ok(steps);
            }
            steps.push(self.step()?);
        }
    }

    fn step(&mut self) -> Result<Step, XPathError> {
        // The two abbreviated steps.
        if self.eat("..") {
            return Ok(Step {
                axis: Axis::Parent,
                test: Test::Anything,
                predicates: Vec::new(),
            });
        }
        if self.eat(".") {
            return Ok(Step {
                axis: Axis::Itself,
                test: Test::Anything,
                predicates: Vec::new(),
            });
        }

        let axis = if self.eat("@") {
            Axis::Attribute
        } else if let Some(Token::Name(name)) = self.peek().cloned()
            && matches!(self.tokens.get(self.at + 1), Some((Token::Symbol("::"), _)))
        {
            let at = self.where_we_are();
            self.at += 2;
            match name.as_str() {
                "child" => Axis::Child,
                "descendant" => Axis::Descendant,
                "descendant-or-self" => Axis::DescendantOrSelf,
                "parent" => Axis::Parent,
                "ancestor" => Axis::Ancestor,
                "ancestor-or-self" => Axis::AncestorOrSelf,
                "following-sibling" => Axis::FollowingSibling,
                "preceding-sibling" => Axis::PrecedingSibling,
                "following" => Axis::Following,
                "preceding" => Axis::Preceding,
                "self" => Axis::Itself,
                "attribute" => Axis::Attribute,
                "namespace" => {
                    return Err(XPathError {
                        message: "the namespace axis is not supported".to_owned(),
                        at,
                    });
                }
                other => {
                    return Err(XPathError {
                        message: format!("{other:?} is not an axis"),
                        at,
                    });
                }
            }
        } else {
            Axis::Child
        };

        let test = self.node_test(axis)?;
        let mut predicates = Vec::new();
        while self.eat("[") {
            predicates.push(self.expr()?);
            self.expect("]")?;
        }
        Ok(Step {
            axis,
            test,
            predicates,
        })
    }

    fn node_test(&mut self, axis: Axis) -> Result<Test, XPathError> {
        if self.eat("*") {
            return Ok(Test::Any);
        }
        let at = self.where_we_are();
        let Some(Token::Name(name)) = self.peek().cloned() else {
            return Err(XPathError {
                message: "a step needs a name, a `*` or a node test".to_owned(),
                at,
            });
        };
        self.at += 1;

        // A name followed by `(` is a node test rather than an element name.
        if matches!(self.peek(), Some(Token::Symbol("("))) {
            self.at += 1;
            let test = match name.as_str() {
                "node" => Test::Anything,
                "text" => Test::Text,
                "comment" => Test::Comment,
                "processing-instruction" => {
                    // Its optional literal argument is accepted and ignored: an
                    // HTML document has no processing instructions to tell apart.
                    if let Some(Token::Literal(_)) = self.peek() {
                        self.at += 1;
                    }
                    Test::ProcessingInstruction
                }
                other => {
                    return Err(XPathError {
                        message: format!("{other}() is not a node test"),
                        at,
                    });
                }
            };
            self.expect(")")?;
            return Ok(test);
        }

        let _ = axis;
        Ok(Test::Name(name))
    }

    /// A literal, a number, a function call or a parenthesised expression.
    ///
    /// `None` when what comes next is none of those, which means it is a step and
    /// the caller should parse one.
    fn primary(&mut self) -> Result<Option<Expr>, XPathError> {
        match self.peek().cloned() {
            Some(Token::Literal(text)) => {
                self.at += 1;
                Ok(Some(Expr::Literal(text)))
            }
            Some(Token::Number(number)) => {
                self.at += 1;
                Ok(Some(Expr::Number(number)))
            }
            Some(Token::Symbol("(")) => {
                self.at += 1;
                let inner = self.expr()?;
                self.expect(")")?;
                Ok(Some(inner))
            }
            // A name followed by `(` is a call — unless it is a node test, which
            // is a step and belongs to the caller.
            Some(Token::Name(name))
                if matches!(self.tokens.get(self.at + 1), Some((Token::Symbol("("), _)))
                    && !matches!(
                        name.as_str(),
                        "node" | "text" | "comment" | "processing-instruction"
                    ) =>
            {
                self.at += 2;
                let mut arguments = Vec::new();
                if !self.eat(")") {
                    loop {
                        arguments.push(self.expr()?);
                        if self.eat(",") {
                            continue;
                        }
                        self.expect(")")?;
                        break;
                    }
                }
                Ok(Some(Expr::Call(name, arguments)))
            }
            _ => Ok(None),
        }
    }
}

// --- evaluation ------------------------------------------------------------

/// Where an expression is being evaluated.
struct Context {
    item: Item,
    position: usize,
    size: usize,
}

struct Engine<'a> {
    document: &'a Document,
    /// Every node's place in document order.
    ///
    /// Built once per evaluation and used to sort and dedupe node-sets, which the
    /// specification requires of every one of them. Computing it by walking up to
    /// the root for each comparison would turn a union of two large sets into a
    /// walk per pair.
    order: HashMap<NodeId, usize>,
}

impl<'a> Engine<'a> {
    fn new(document: &'a Document) -> Self {
        let mut order = HashMap::new();
        let mut stack = vec![document.root()];
        let mut next = 0;
        // Depth-first, children pushed in reverse so they come off in order.
        while let Some(node) = stack.pop() {
            order.insert(node, next);
            next += 1;
            let children: Vec<NodeId> = document.children(node).collect();
            stack.extend(children.into_iter().rev());
        }
        Self { document, order }
    }

    fn place(&self, node: NodeId) -> usize {
        self.order.get(&node).copied().unwrap_or(usize::MAX)
    }

    /// Put a node-set in document order and drop the duplicates.
    ///
    /// An attribute sorts with its element and after it, which is the order the
    /// specification puts attributes in.
    fn tidy(&self, mut items: Vec<Item>) -> Vec<Item> {
        items.sort_by_key(|item| {
            (
                self.place(item.anchor()),
                match item {
                    Item::Node(_) => 0,
                    Item::Attribute { .. } => 1,
                },
                match item {
                    Item::Attribute { name, .. } => name.clone(),
                    Item::Node(_) => String::new(),
                },
            )
        });
        items.dedup();
        items
    }

    fn eval(&self, expression: &Expr, context: &Context) -> Result<Value, XPathError> {
        match expression {
            Expr::Literal(text) => Ok(Value::Text(text.clone())),
            Expr::Number(number) => Ok(Value::Number(*number)),
            Expr::Negate(inner) => Ok(Value::Number(-self.number(&self.eval(inner, context)?))),
            Expr::Or(left, right) => {
                // Short-circuit, which the specification requires and which is
                // what makes `@x and starts-with(@x, "y")` safe to write.
                if self.boolean(&self.eval(left, context)?) {
                    return Ok(Value::Boolean(true));
                }
                Ok(Value::Boolean(self.boolean(&self.eval(right, context)?)))
            }
            Expr::And(left, right) => {
                if !self.boolean(&self.eval(left, context)?) {
                    return Ok(Value::Boolean(false));
                }
                Ok(Value::Boolean(self.boolean(&self.eval(right, context)?)))
            }
            Expr::Compare(operator, left, right) => {
                let left = self.eval(left, context)?;
                let right = self.eval(right, context)?;
                Ok(Value::Boolean(self.compare(*operator, &left, &right)))
            }
            Expr::Arith(operator, left, right) => {
                let left = self.number(&self.eval(left, context)?);
                let right = self.number(&self.eval(right, context)?);
                Ok(Value::Number(match operator {
                    Arith::Add => left + right,
                    Arith::Subtract => left - right,
                    Arith::Multiply => left * right,
                    Arith::Divide => left / right,
                    Arith::Modulo => left % right,
                }))
            }
            Expr::Union(left, right) => {
                let mut items = self.nodes(&self.eval(left, context)?, "the left of a `|`")?;
                items.extend(self.nodes(&self.eval(right, context)?, "the right of a `|`")?);
                Ok(Value::Nodes(self.tidy(items)))
            }
            Expr::Call(name, arguments) => self.call(name, arguments, context),
            Expr::Path {
                absolute,
                start,
                steps,
            } => {
                let mut items = match (absolute, start) {
                    (true, _) => vec![Item::Node(self.document.root())],
                    (false, Some(start)) => {
                        self.nodes(&self.eval(start, context)?, "the start of a path")?
                    }
                    (false, None) => vec![context.item.clone()],
                };
                for step in steps {
                    items = self.step(step, &items)?;
                }
                Ok(Value::Nodes(items))
            }
        }
    }

    /// One step, applied to every node the step before it selected.
    fn step(&self, step: &Step, from: &[Item]) -> Result<Vec<Item>, XPathError> {
        let mut out = Vec::new();
        for item in from {
            let mut reached: Vec<Item> = self
                .along(step.axis, item)
                .into_iter()
                .filter(|candidate| self.matches(&step.test, candidate, step.axis))
                .collect();

            // Predicates are applied in order, and each one sees the positions
            // the one before it left — which is why `[1][2]` selects nothing.
            for predicate in &step.predicates {
                reached = self.filter(&reached, predicate, step.axis)?;
            }
            out.extend(reached);
        }
        Ok(self.tidy(out))
    }

    /// Keep the members a predicate says yes to.
    fn filter(
        &self,
        items: &[Item],
        predicate: &Expr,
        axis: Axis,
    ) -> Result<Vec<Item>, XPathError> {
        let size = items.len();
        let mut kept = Vec::new();
        for (index, item) in items.iter().enumerate() {
            // On a reverse axis position counts from the context node outwards,
            // so the *last* member of a document-ordered list is at position one.
            let position = if axis.reverse() {
                size - index
            } else {
                index + 1
            };
            let context = Context {
                item: item.clone(),
                position,
                size,
            };
            let verdict = self.eval(predicate, &context)?;
            // A predicate that is a bare number is a position test; anything else
            // is taken as a boolean. `[1]` and `[position()=1]` are the same
            // thing, and `[@id]` is not a position at all.
            let keep = match verdict {
                Value::Number(wanted) => {
                    // Compared as a float on purpose: `[1.5]` matches nothing,
                    // which is what a non-integer position means.
                    (position as f64 - wanted).abs() < f64::EPSILON
                }
                other => self.boolean(&other),
            };
            if keep {
                kept.push(item.clone());
            }
        }
        Ok(kept)
    }

    /// Everything on `axis` from `item`, in document order.
    fn along(&self, axis: Axis, item: &Item) -> Vec<Item> {
        let node = item.anchor();
        // Every axis but `self` and `parent` is empty from an attribute in the
        // sense a locator uses, and both of those are about the element it is on.
        let from_attribute = matches!(item, Item::Attribute { .. });

        match axis {
            Axis::Itself => vec![item.clone()],
            Axis::Attribute => {
                if from_attribute {
                    return Vec::new();
                }
                self.document
                    .get(node)
                    .and_then(|data| data.element())
                    .map(|element| {
                        element
                            .attrs
                            .iter()
                            .map(|attribute| Item::Attribute {
                                owner: node,
                                name: attribute.name.local.as_ref().to_owned(),
                                value: attribute.value.to_string(),
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            }
            Axis::Child => {
                if from_attribute {
                    return Vec::new();
                }
                self.document.children(node).map(Item::Node).collect()
            }
            Axis::Parent => {
                if from_attribute {
                    return vec![Item::Node(node)];
                }
                self.document
                    .get(node)
                    .and_then(|data| data.parent)
                    .map(|parent| vec![Item::Node(parent)])
                    .unwrap_or_default()
            }
            Axis::Descendant => self.descendants(node, false),
            Axis::DescendantOrSelf => {
                if from_attribute {
                    return vec![item.clone()];
                }
                self.descendants(node, true)
            }
            Axis::Ancestor => self.ancestors(node, false),
            Axis::AncestorOrSelf => {
                let mut out = if from_attribute {
                    vec![item.clone()]
                } else {
                    vec![Item::Node(node)]
                };
                out.extend(self.ancestors(node, false));
                out
            }
            Axis::FollowingSibling | Axis::PrecedingSibling => {
                if from_attribute {
                    return Vec::new();
                }
                let Some(parent) = self.document.get(node).and_then(|data| data.parent) else {
                    return Vec::new();
                };
                let siblings: Vec<NodeId> = self.document.children(parent).collect();
                let Some(index) = siblings.iter().position(|sibling| *sibling == node) else {
                    return Vec::new();
                };
                let taken = if axis == Axis::FollowingSibling {
                    &siblings[index + 1..]
                } else {
                    &siblings[..index]
                };
                taken.iter().copied().map(Item::Node).collect()
            }
            Axis::Following | Axis::Preceding => {
                // Everything after the node in document order that is not a
                // descendant of it, and the mirror for `preceding` — which also
                // excludes the ancestors.
                let place = self.place(node);
                let descendants: Vec<NodeId> = self
                    .descendants(node, true)
                    .into_iter()
                    .filter_map(|item| match item {
                        Item::Node(node) => Some(node),
                        Item::Attribute { .. } => None,
                    })
                    .collect();
                let ancestors: Vec<NodeId> = self
                    .ancestors(node, false)
                    .into_iter()
                    .filter_map(|item| match item {
                        Item::Node(node) => Some(node),
                        Item::Attribute { .. } => None,
                    })
                    .collect();

                let mut out: Vec<(usize, NodeId)> = self
                    .order
                    .iter()
                    .filter(|(candidate, at)| {
                        let after = **at > place;
                        let wanted = if axis == Axis::Following {
                            after
                        } else {
                            **at < place
                        };
                        wanted
                            && !descendants.contains(candidate)
                            && (axis == Axis::Following || !ancestors.contains(candidate))
                    })
                    .map(|(node, at)| (*at, *node))
                    .collect();
                out.sort_unstable();
                out.into_iter().map(|(_, node)| Item::Node(node)).collect()
            }
        }
    }

    fn descendants(&self, node: NodeId, include_self: bool) -> Vec<Item> {
        let mut out = Vec::new();
        if include_self {
            out.push(Item::Node(node));
        }
        let mut stack: Vec<NodeId> = self.document.children(node).collect();
        stack.reverse();
        while let Some(next) = stack.pop() {
            out.push(Item::Node(next));
            let children: Vec<NodeId> = self.document.children(next).collect();
            stack.extend(children.into_iter().rev());
        }
        out
    }

    fn ancestors(&self, node: NodeId, include_self: bool) -> Vec<Item> {
        let mut out = Vec::new();
        if include_self {
            out.push(Item::Node(node));
        }
        let mut at = self.document.get(node).and_then(|data| data.parent);
        while let Some(parent) = at {
            out.push(Item::Node(parent));
            at = self.document.get(parent).and_then(|data| data.parent);
        }
        out
    }

    /// Whether a node test says yes to what an axis reached.
    fn matches(&self, test: &Test, item: &Item, axis: Axis) -> bool {
        match item {
            Item::Attribute { name, .. } => match test {
                Test::Any | Test::Anything => true,
                // Attribute names are compared case-insensitively for the same
                // reason tag names are: the parser lower-cases them and a locator
                // is written the way the markup reads.
                Test::Name(wanted) => name.eq_ignore_ascii_case(wanted),
                _ => false,
            },
            Item::Node(node) => {
                let Some(data) = self.document.get(*node) else {
                    return false;
                };
                match (test, &data.data) {
                    (Test::Anything, _) => true,
                    (Test::Text, NodeData::Text(_)) => true,
                    (Test::Comment, NodeData::Comment(_)) => true,
                    (Test::ProcessingInstruction, _) => false,
                    // `*` is every element — and on the attribute axis it is
                    // every attribute, which the arm above already answered.
                    (Test::Any, NodeData::Element(_)) => true,
                    (Test::Name(wanted), NodeData::Element(element)) => {
                        let _ = axis;
                        element.name.local.as_ref().eq_ignore_ascii_case(wanted)
                    }
                    _ => false,
                }
            }
        }
    }

    // --- the four conversions ---------------------------------------------

    /// A value as a node-set, or a refusal naming where it was wanted.
    fn nodes(&self, value: &Value, where_it_was: &str) -> Result<Vec<Item>, XPathError> {
        match value {
            Value::Nodes(items) => Ok(items.clone()),
            _ => Err(XPathError {
                message: format!("{where_it_was} has to be a node-set"),
                at: 0,
            }),
        }
    }

    fn boolean(&self, value: &Value) -> bool {
        match value {
            Value::Boolean(boolean) => *boolean,
            Value::Number(number) => *number != 0.0 && !number.is_nan(),
            Value::Text(text) => !text.is_empty(),
            Value::Nodes(items) => !items.is_empty(),
        }
    }

    fn number(&self, value: &Value) -> f64 {
        match value {
            Value::Number(number) => *number,
            Value::Boolean(boolean) => f64::from(u8::from(*boolean)),
            Value::Text(text) => text.trim().parse().unwrap_or(f64::NAN),
            Value::Nodes(_) => self.string(value).trim().parse().unwrap_or(f64::NAN),
        }
    }

    fn string(&self, value: &Value) -> String {
        match value {
            Value::Text(text) => text.clone(),
            Value::Boolean(boolean) => if *boolean { "true" } else { "false" }.to_owned(),
            Value::Number(number) => format_number(*number),
            // A node-set's string-value is that of its *first* node in document
            // order, and the empty string when it has none.
            Value::Nodes(items) => items
                .first()
                .map(|item| self.item_string(item))
                .unwrap_or_default(),
        }
    }

    /// The string-value of one node: the text under it, run together.
    fn item_string(&self, item: &Item) -> String {
        match item {
            Item::Attribute { value, .. } => value.clone(),
            Item::Node(node) => {
                let Some(data) = self.document.get(*node) else {
                    return String::new();
                };
                match &data.data {
                    NodeData::Text(text) => text.to_string(),
                    NodeData::Comment(text) => text.to_string(),
                    NodeData::Doctype { name, .. } => name.to_string(),
                    // An element or the document is everything under it.
                    NodeData::Element(_) | NodeData::Document => {
                        let mut out = String::new();
                        for reached in self.descendants(*node, false) {
                            if let Item::Node(child) = reached
                                && let Some(NodeData::Text(text)) =
                                    self.document.get(child).map(|data| &data.data)
                            {
                                out.push_str(text);
                            }
                        }
                        out
                    }
                }
            }
        }
    }

    /// Compare two values, by the rules that make `//div[@class="x"]` work.
    ///
    /// A comparison with a node-set on either side is *existential*: it is true
    /// when some node in the set compares true. That is why an element with two
    /// classes matches a test against either of them, and it is the rule most
    /// often got wrong by an implementation that converts first and compares
    /// after.
    fn compare(&self, operator: Compare, left: &Value, right: &Value) -> bool {
        let equality = matches!(operator, Compare::Eq | Compare::Ne);

        match (left, right) {
            (Value::Nodes(ours), Value::Nodes(theirs)) => ours.iter().any(|one| {
                theirs.iter().any(|other| {
                    self.compare_scalars(
                        operator,
                        &Value::Text(self.item_string(one)),
                        &Value::Text(self.item_string(other)),
                    )
                })
            }),
            (Value::Nodes(items), other) | (other, Value::Nodes(items)) => {
                // The node-set stays on the side it was written, so `<` does not
                // silently become `>`.
                let flipped = matches!(right, Value::Nodes(_));
                items.iter().any(|item| {
                    let ours = Value::Text(self.item_string(item));
                    let (one, two) = if flipped {
                        (other.clone(), ours)
                    } else {
                        (ours, other.clone())
                    };
                    // Against a boolean it is a boolean comparison, against a
                    // number a numeric one, and against a string a string one —
                    // for equality. A relational operator is always numeric.
                    if !equality {
                        return self.compare_scalars(operator, &one, &two);
                    }
                    match other {
                        Value::Boolean(_) => {
                            let ours =
                                Value::Boolean(self.boolean(if flipped { &two } else { &one }));
                            self.compare_scalars(operator, &ours, other)
                        }
                        Value::Number(_) => self.compare_scalars(
                            operator,
                            &Value::Number(self.number(&one)),
                            &Value::Number(self.number(&two)),
                        ),
                        _ => self.compare_scalars(operator, &one, &two),
                    }
                })
            }
            _ => self.compare_scalars(operator, left, right),
        }
    }

    fn compare_scalars(&self, operator: Compare, left: &Value, right: &Value) -> bool {
        // Relational operators are numeric whatever the operands are.
        if !matches!(operator, Compare::Eq | Compare::Ne) {
            let (left, right) = (self.number(left), self.number(right));
            return match operator {
                Compare::Lt => left < right,
                Compare::Le => left <= right,
                Compare::Gt => left > right,
                Compare::Ge => left >= right,
                Compare::Eq | Compare::Ne => unreachable!(),
            };
        }

        // Equality converts to whichever type is "widest" of the two: boolean
        // first, then number, then string.
        let same = match (left, right) {
            (Value::Boolean(_), _) | (_, Value::Boolean(_)) => {
                self.boolean(left) == self.boolean(right)
            }
            (Value::Number(_), _) | (_, Value::Number(_)) => {
                let (left, right) = (self.number(left), self.number(right));
                left == right
            }
            _ => self.string(left) == self.string(right),
        };
        if operator == Compare::Eq { same } else { !same }
    }

    // --- functions ---------------------------------------------------------

    fn call(&self, name: &str, arguments: &[Expr], context: &Context) -> Result<Value, XPathError> {
        let arity = |wanted: std::ops::RangeInclusive<usize>| -> Result<(), XPathError> {
            if wanted.contains(&arguments.len()) {
                return Ok(());
            }
            Err(XPathError {
                message: format!(
                    "{name}() takes {}–{} arguments and was given {}",
                    wanted.start(),
                    wanted.end(),
                    arguments.len()
                ),
                at: 0,
            })
        };
        // The argument as each of the four kinds, evaluated on demand.
        let value =
            |index: usize| -> Result<Value, XPathError> { self.eval(&arguments[index], context) };
        let text = |index: usize| -> Result<String, XPathError> {
            if index >= arguments.len() {
                // A missing string argument means the context node, which is what
                // `string-length()` and `normalize-space()` default to.
                return Ok(self.item_string(&context.item));
            }
            Ok(self.string(&value(index)?))
        };
        let number = |index: usize| -> Result<f64, XPathError> { Ok(self.number(&value(index)?)) };

        match name {
            "position" => {
                arity(0..=0)?;
                Ok(Value::Number(context.position as f64))
            }
            "last" => {
                arity(0..=0)?;
                Ok(Value::Number(context.size as f64))
            }
            "count" => {
                arity(1..=1)?;
                Ok(Value::Number(
                    self.nodes(&value(0)?, "the argument of count()")?.len() as f64,
                ))
            }
            "name" | "local-name" => {
                arity(0..=1)?;
                let item = if arguments.is_empty() {
                    Some(context.item.clone())
                } else {
                    self.nodes(&value(0)?, &format!("the argument of {name}()"))?
                        .first()
                        .cloned()
                };
                Ok(Value::Text(match item {
                    Some(Item::Attribute { name, .. }) => name,
                    Some(Item::Node(node)) => self
                        .document
                        .get(node)
                        .and_then(|data| data.element())
                        .map(|element| element.name.local.as_ref().to_owned())
                        .unwrap_or_default(),
                    None => String::new(),
                }))
            }
            "string" => {
                arity(0..=1)?;
                Ok(Value::Text(text(0)?))
            }
            "concat" => {
                arity(2..=usize::MAX)?;
                let mut out = String::new();
                for index in 0..arguments.len() {
                    out.push_str(&self.string(&value(index)?));
                }
                Ok(Value::Text(out))
            }
            "starts-with" => {
                arity(2..=2)?;
                Ok(Value::Boolean(text(0)?.starts_with(&text(1)?)))
            }
            "ends-with" => {
                // Not in 1.0, but every driver writes it and the alternative is a
                // `substring` expression nobody reads twice.
                arity(2..=2)?;
                Ok(Value::Boolean(text(0)?.ends_with(&text(1)?)))
            }
            "contains" => {
                arity(2..=2)?;
                Ok(Value::Boolean(text(0)?.contains(&text(1)?)))
            }
            "substring-before" => {
                arity(2..=2)?;
                let (haystack, needle) = (text(0)?, text(1)?);
                Ok(Value::Text(
                    haystack
                        .find(&needle)
                        .map(|at| haystack[..at].to_owned())
                        .unwrap_or_default(),
                ))
            }
            "substring-after" => {
                arity(2..=2)?;
                let (haystack, needle) = (text(0)?, text(1)?);
                Ok(Value::Text(
                    haystack
                        .find(&needle)
                        .map(|at| haystack[at + needle.len()..].to_owned())
                        .unwrap_or_default(),
                ))
            }
            "substring" => {
                arity(2..=3)?;
                // One-based, and rounded, which is what makes `substring(s, 0, 3)`
                // return two characters rather than three.
                let characters: Vec<char> = text(0)?.chars().collect();
                let from = number(1)?.round();
                let length = if arguments.len() == 3 {
                    number(2)?.round()
                } else {
                    f64::INFINITY
                };
                let start = from.max(1.0);
                let end = from + length;
                let taken: String = characters
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| {
                        let place = *index as f64 + 1.0;
                        place >= start && place < end
                    })
                    .map(|(_, character)| *character)
                    .collect();
                Ok(Value::Text(taken))
            }
            "string-length" => {
                arity(0..=1)?;
                Ok(Value::Number(text(0)?.chars().count() as f64))
            }
            "normalize-space" => {
                arity(0..=1)?;
                Ok(Value::Text(
                    text(0)?.split_whitespace().collect::<Vec<_>>().join(" "),
                ))
            }
            "translate" => {
                arity(3..=3)?;
                let (subject, from, to) = (text(0)?, text(1)?, text(2)?);
                let from: Vec<char> = from.chars().collect();
                let to: Vec<char> = to.chars().collect();
                Ok(Value::Text(
                    subject
                        .chars()
                        .filter_map(|character| {
                            match from.iter().position(|one| *one == character) {
                                // A character with no replacement is removed, which is
                                // how `translate(s, "abc", "")` deletes.
                                Some(index) => to.get(index).copied(),
                                None => Some(character),
                            }
                        })
                        .collect(),
                ))
            }
            "not" => {
                arity(1..=1)?;
                Ok(Value::Boolean(!self.boolean(&value(0)?)))
            }
            "boolean" => {
                arity(1..=1)?;
                Ok(Value::Boolean(self.boolean(&value(0)?)))
            }
            "true" => {
                arity(0..=0)?;
                Ok(Value::Boolean(true))
            }
            "false" => {
                arity(0..=0)?;
                Ok(Value::Boolean(false))
            }
            "number" => {
                arity(0..=1)?;
                if arguments.is_empty() {
                    return Ok(Value::Number(
                        self.item_string(&context.item)
                            .trim()
                            .parse()
                            .unwrap_or(f64::NAN),
                    ));
                }
                Ok(Value::Number(number(0)?))
            }
            "id" => {
                arity(1..=1)?;
                // Every whitespace-separated token, each naming at most one
                // element. A node-set argument contributes the string-value of
                // *each* of its members, which is what makes `id(//x/@refs)`
                // work — and is the whole reason this is not a selector.
                let wanted = match value(0)? {
                    Value::Nodes(items) => items
                        .iter()
                        .flat_map(|item| {
                            self.item_string(item)
                                .split_whitespace()
                                .map(str::to_owned)
                                .collect::<Vec<_>>()
                        })
                        .collect::<Vec<_>>(),
                    other => self
                        .string(&other)
                        .split_whitespace()
                        .map(str::to_owned)
                        .collect(),
                };
                let mut found = Vec::new();
                for item in self.descendants(self.document.root(), false) {
                    let Item::Node(node) = item else { continue };
                    let Some(id) = self
                        .document
                        .get(node)
                        .and_then(|data| data.element())
                        .and_then(crate::node::ElementData::id)
                    else {
                        continue;
                    };
                    if wanted.iter().any(|one| one == id) {
                        found.push(Item::Node(node));
                    }
                }
                Ok(Value::Nodes(self.tidy(found)))
            }
            "lang" => {
                arity(1..=1)?;
                let wanted = self.string(&value(0)?).to_lowercase();
                // The nearest `xml:lang` or `lang` at or above the context node,
                // matched as a prefix on a hyphen: `lang("en")` is true of
                // `en-GB` and false of `english`.
                let mut at = Some(context.item.anchor());
                while let Some(node) = at {
                    if let Some(found) = self
                        .document
                        .get(node)
                        .and_then(|data| data.element())
                        .and_then(|element| {
                            element.attr("xml:lang").or_else(|| element.attr("lang"))
                        })
                    {
                        let found = found.to_lowercase();
                        return Ok(Value::Boolean(
                            found == wanted || found.starts_with(&format!("{wanted}-")),
                        ));
                    }
                    at = self.document.get(node).and_then(|data| data.parent);
                }
                Ok(Value::Boolean(false))
            }
            "sum" => {
                arity(1..=1)?;
                let items = self.nodes(&value(0)?, "the argument of sum()")?;
                Ok(Value::Number(
                    items
                        .iter()
                        .map(|item| self.item_string(item).trim().parse().unwrap_or(f64::NAN))
                        .sum(),
                ))
            }
            "floor" => {
                arity(1..=1)?;
                Ok(Value::Number(number(0)?.floor()))
            }
            "ceiling" => {
                arity(1..=1)?;
                Ok(Value::Number(number(0)?.ceil()))
            }
            "round" => {
                arity(1..=1)?;
                Ok(Value::Number(number(0)?.round()))
            }
            other => Err(XPathError {
                message: format!("{other}() is not a function this engine has"),
                at: 0,
            }),
        }
    }
}

/// A number as XPath writes one: no trailing `.0`, and the two special names.
fn format_number(number: f64) -> String {
    if number.is_nan() {
        return "NaN".to_owned();
    }
    if number.is_infinite() {
        return if number > 0.0 {
            "Infinity"
        } else {
            "-Infinity"
        }
        .to_owned();
    }
    if number == number.trunc() && number.abs() < 1e21 {
        return format!("{}", number as i64);
    }
    format!("{number}")
}
