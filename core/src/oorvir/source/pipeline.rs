use crate::diagnostic::OORVError;
use crate::oorvast::OORVAst;
use crate::oorvir::source::core::*;

#[derive(Debug, Clone)]
pub struct OORVAstParser {
    // NOTE: these fields are unused — the public API is the static parse_for_ir method.
    // ast: OORVAst,
    // ir: OORVIR,
}

impl OORVAstParser {
    /// Converts a parsed OORV AST to the fully analysed and condensed IR.
    ///
    /// Runs the complete analysis pipeline:
    ///   1. AST → source_ir (name binding + structural lowering)
    ///   2. Type inference (value types + pacing types)
    ///   3. Dependency analysis
    ///   4. Layer / scheduling pass
    ///   5. Memory-bound computation
    ///   6. Condensation to the refined_ir form
    pub fn parse_for_ir(ast: OORVAst, _source: String) -> Result<OORVIr1, OORVError> {
        let initial = OORVIr1::from_ast(ast)?
            .run_type_pass()?
            .run_dep_pass()?
            .run_layer_pass()?
            .run_memory_pass()?
            .finalize_ir()?;

        Ok(initial)
    }
}
