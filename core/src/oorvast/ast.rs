use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::hash::Hash;
use std::rc::Rc;

// Fundamental node identifier, allocated incrementally per AST
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct AstNodeId(pub u32);

impl AstNodeId {
    pub fn new(x: usize) -> AstNodeId {
        assert!(x < (u32::MAX as usize));
        AstNodeId(x as u32)
    }
}

// Source byte-range carried by every AST node
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SourceSpan {
    Direct { start: usize, end: usize },
    Indirect { start: usize, end: usize },
    Unknown,
}

impl Default for SourceSpan {
    fn default() -> Self {
        SourceSpan::Unknown
    }
}

impl SourceSpan {
    pub fn start(&self) -> usize {
        match self {
            SourceSpan::Direct { start, .. } => *start,
            SourceSpan::Indirect { start, .. } => *start,
            SourceSpan::Unknown => 0,
        }
    }

    pub fn end(&self) -> usize {
        match self {
            SourceSpan::Direct { end, .. } => *end,
            SourceSpan::Indirect { end, .. } => *end,
            SourceSpan::Unknown => 0,
        }
    }

    // Merge two spans into the smallest enclosing range
    pub fn union(&self, other: &SourceSpan) -> SourceSpan {
        match (self, other) {
            (
                SourceSpan::Direct {
                    start: a_s,
                    end: a_e,
                },
                SourceSpan::Direct {
                    start: b_s,
                    end: b_e,
                },
            ) => SourceSpan::Direct {
                start: std::cmp::min(*a_s, *b_s),
                end: std::cmp::max(*a_e, *b_e),
            },
            (
                SourceSpan::Direct {
                    start: a_s,
                    end: a_e,
                },
                SourceSpan::Indirect {
                    start: b_s,
                    end: b_e,
                },
            )
            | (
                SourceSpan::Indirect {
                    start: b_s,
                    end: b_e,
                },
                SourceSpan::Direct {
                    start: a_s,
                    end: a_e,
                },
            )
            | (
                SourceSpan::Indirect {
                    start: a_s,
                    end: a_e,
                },
                SourceSpan::Indirect {
                    start: b_s,
                    end: b_e,
                },
            ) => SourceSpan::Indirect {
                start: std::cmp::min(*a_s, *b_s),
                end: std::cmp::max(*a_e, *b_e),
            },
            (SourceSpan::Unknown, other) | (other, SourceSpan::Unknown) => other.clone(),
        }
    }
}

// A string identifier carrying its source location
#[derive(Debug, Clone, Eq, Serialize, Deserialize, PartialOrd, Ord)]
pub struct Identifier {
    pub name: String,
    pub span: SourceSpan,
}

impl Identifier {
    pub(crate) fn new(name: String, span: SourceSpan) -> Identifier {
        Identifier { name, span }
    }
}

impl PartialEq for Identifier {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Hash for Identifier {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

// Time unit for frequency/duration literals
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TimeUnit {
    Nanosecond,
    Microsecond,
    Millisecond,
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Year,
}

// Token-level literal value
#[derive(Debug, Clone, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum LiteralKind {
    Text(String),
    RawText(String),
    Number(String, Option<String>),
    Boolean(bool),
    Tuple(Vec<TokenLiteral>),
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct TokenLiteral {
    pub kind: LiteralKind,
    pub node_id: AstNodeId,
    pub span: SourceSpan,
}

// Arithmetic, logical, and bitwise binary operators
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
    And,
    Or,
    BitXor,
    BitAnd,
    BitOr,
    Shl,
    Shr,
    Eq,
    Lt,
    Le,
    Ne,
    Ge,
    Gt,
}

// Unary prefix operators
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum UnaryOp {
    Not,
    Neg,
    BitNot,
}

// How a stream value is accessed (strict, cached, etc.)
#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash, PartialOrd, Ord)]
pub enum AccessMode {
    Strict,
    Cached,
    Get,
    Fresh,
}

// Quantifier keyword
#[derive(Debug, Clone, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum Quantifier {
    Forall,
    Exists,
}

// Temporal / index-based offset kind
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Shift {
    Discrete(i16),
}

// Object instance selection mode
#[derive(Debug, Clone, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum InstanceSelection {
    Fresh,
    All,
}

// Named function / method reference including argument labels
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash, PartialOrd, Ord)]
pub struct FuncLabel {
    pub name: Identifier,
    pub arg_names: Vec<Option<Identifier>>,
}

// A type annotation (named, tuple, or optional)
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct ValueType {
    pub kind: ValueTypeKind,
    pub node_id: AstNodeId,
    pub span: SourceSpan,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum ValueTypeKind {
    Named(String),
    Tuple(Vec<ValueType>),
    Optional(Box<ValueType>),
}

// Expression variant  the core recursive expression type
#[allow(clippy::large_enum_variant, clippy::vec_box)]
#[derive(Debug, Clone, Hash, Eq, PartialEq, PartialOrd, Ord)]
pub enum ExprVariant {
    Literal(TokenLiteral),
    Identifier(Identifier),
    SignalAccess(Box<ExprNode>, AccessMode),
    Default(Box<ExprNode>, Box<ExprNode>),
    Shift(Box<ExprNode>, Shift),
    Binary(BinaryOp, Box<ExprNode>, Box<ExprNode>),
    Unary(UnaryOp, Box<ExprNode>),
    Ite(Box<ExprNode>, Box<ExprNode>, Box<ExprNode>),
    Bracket(Box<ExprNode>),
    MissingExpr,
    Tuple(Vec<ExprNode>),
    Field(Box<ExprNode>, Identifier),
    Method(Box<ExprNode>, FuncLabel, Vec<ValueType>, Vec<ExprNode>),
    Function(FuncLabel, Vec<ValueType>, Vec<ExprNode>),
    Quantified(Quantifier, Vec<Identifier>, Vec<Identifier>, Box<ExprNode>),
}

// An expression node with its source location
#[derive(Debug, Clone, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct ExprNode {
    pub kind: ExprVariant,
    pub node_id: AstNodeId,
    pub span: SourceSpan,
}

// Activation / pacing annotation for streams
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PacingNode {
    NotAnnotated(SourceSpan),
    Global(ExprNode),
    Local(ExprNode),
    Unspecified(ExprNode),
}

// start-of-stream declaration
#[derive(Debug, Clone, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct StartDecl {
    pub pacing: PacingNode,
    pub condition: Option<ExprNode>,
    pub expression: Option<ExprNode>,
    pub node_id: AstNodeId,
    pub span: SourceSpan,
}

// eval / update declaration for a stream
#[derive(Debug, Clone, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct EvalDecl {
    pub pacing: PacingNode,
    pub condition: Option<ExprNode>,
    pub expression: Option<ExprNode>,
    pub node_id: AstNodeId,
    pub span: SourceSpan,
}

// end-of-stream declaration
#[derive(Debug, Clone, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct EndDecl {
    pub pacing: PacingNode,
    pub condition: ExprNode,
    pub node_id: AstNodeId,
    pub span: SourceSpan,
}

// Single parameter in a function or method signature
#[derive(Debug, Clone, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct ParamDecl {
    pub name: Identifier,
    pub annotation: Option<ValueType>,
    pub position: usize,
    pub node_id: AstNodeId,
    pub span: SourceSpan,
}

// A local let binding or expression statement inside a method
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LetDecl {
    pub name: Identifier,
    pub expr: ExprNode,
    pub span: SourceSpan,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum MethodStmt {
    Let(LetDecl),
    Expr(ExprNode),
}

// Body of a function declaration (let bindings + return expression)
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MethodBody {
    pub decls: Vec<MethodStmt>,
    pub ret: Option<ExprNode>,
    pub span: SourceSpan,
}

// Identifies whether a constraint targets an output stream or an alarm
#[derive(Debug, Clone, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum ConstrainKind {
    Output(Identifier),
    Alarm,
}

// A fully resolved constraint declaration
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Constrain {
    pub kind: ConstrainKind,
    pub annotation: Option<ValueType>,
    pub params: Vec<Rc<ParamDecl>>,
    pub override_flag: bool,
    pub module_name: Option<Identifier>,
    pub class_name: Option<Identifier>,
    pub start: Option<StartDecl>,
    pub eval: Vec<EvalDecl>,
    pub end: Option<EndDecl>,
    pub level: Option<Identifier>,
    pub node_id: AstNodeId,
    pub span: SourceSpan,
}

impl Constrain {
    pub fn name(&self) -> Option<&Identifier> {
        match &self.kind {
            ConstrainKind::Output(n) => Some(n),
            ConstrainKind::Alarm => None,
        }
    }
}

// A signal (stream) declaration
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Signal {
    pub name: Identifier,
    pub annotation: ValueType,
    pub module_name: Option<Identifier>,
    pub class_name: Option<Identifier>,
    pub params: Vec<Rc<ParamDecl>>,
    pub node_id: AstNodeId,
    pub span: SourceSpan,
}

// A top-level class / type declaration
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClassDecl {
    pub name: Identifier,
    pub module_name: Option<Identifier>,
    pub base_class: Option<Identifier>,
    pub signals: Vec<Rc<Signal>>,
    pub constrains: Vec<Rc<Constrain>>,
    pub uses: BTreeSet<String>,
    pub node_id: AstNodeId,
    pub span: SourceSpan,
}

// A top-level constant declaration
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ConstDecl {
    pub name: Identifier,
    pub annotation: Option<ValueType>,
    pub value: TokenLiteral,
    pub node_id: AstNodeId,
    pub span: SourceSpan,
}

// A global function declaration
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct GlobalMethodDecl {
    pub node_id: AstNodeId,
    pub name: Identifier,
    pub module_name: Option<Identifier>,
    pub params: Vec<Rc<ParamDecl>>,
    pub return_type: Option<ValueType>,
    pub body: MethodBody,
    pub span: SourceSpan,
}

// A world / scene member field declaration
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Member {
    pub name: Identifier,
    pub annotation: Option<ValueType>,
    pub uses: BTreeSet<String>,
    pub ty_name: Identifier,
    pub params: Vec<Rc<ParamDecl>>,
    pub node_id: AstNodeId,
    pub span: SourceSpan,
}

// An include directive
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct IncludeDecl {
    pub file_name: TokenLiteral,
    pub node_id: AstNodeId,
    pub span: SourceSpan,
}

// Root of the parsed OORV AST
#[derive(Debug, Default, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct OORVAst {
    pub includes: Vec<IncludeDecl>,
    pub constants: Vec<Rc<ConstDecl>>,
    pub classes: Vec<Rc<ClassDecl>>,
    pub functions: Vec<GlobalMethodDecl>,
    pub signals: Vec<Rc<Signal>>,
    pub constrains: Vec<Rc<Constrain>>,
    pub members: Vec<Rc<Member>>,
    pub nodecnts: RefCell<AstNodeId>,
}

impl OORVAst {
    pub fn default() -> Self {
        OORVAst {
            includes: Vec::new(),
            constants: Vec::new(),
            classes: Vec::new(),
            functions: Vec::new(),
            signals: Vec::new(),
            constrains: Vec::new(),
            members: Vec::new(),
            nodecnts: RefCell::new(AstNodeId::default()),
        }
    }

    pub(crate) fn alloc_node_id(&self) -> AstNodeId {
        let mut cnt = self.nodecnts.borrow_mut();
        let current = *cnt;
        cnt.0 += 1;
        current
    }
}
