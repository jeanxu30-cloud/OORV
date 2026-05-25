pub mod diagnostic;
pub mod oorvast;
pub mod parse;
pub use oorvast as ast;
pub mod oorvir;
pub use oorvir as ir;
pub mod runtime;

use std::path::Path;

use diagnostic::{Diagnostic, OORVError};
use oorvir::refined::OORVIR;
use oorvir::source::pipeline::OORVAstParser;
use oorvir::source::OORVIr1;
use parse::OORVSpecParser;

/// Parse an OORV source string and run the complete front-end analysis pipeline.
///
/// The returned source IR has passed name binding, value/pacing checks,
/// dependency analysis, scheduling/layering, and storage-bound analysis. This
/// is the shared entry point for tools that need access to the executable
/// front-end representation before choosing a concrete backend.
pub fn compile_source_ir_text(spec_text: String, source: String) -> Result<OORVIr1, OORVError> {
    let ast = OORVSpecParser::parse_for_ast(spec_text, source.clone())?;
    OORVAstParser::parse_for_ir(ast, source)
}

/// Parse, analyse, and lower an OORV source string to the refined executable IR.
pub fn compile_refined_text(spec_text: String, source: String) -> Result<OORVIR, OORVError> {
    let source_ir = compile_source_ir_text(spec_text, source)?;
    Ok(OORVIR::compile_from_source(source_ir))
}

/// Read an OORV specification file and compile it to the refined executable IR.
pub fn compile_refined_file(path: impl AsRef<Path>) -> Result<OORVIR, OORVError> {
    let path = path.as_ref();
    let raw = std::fs::read_to_string(path).map_err(|e| {
        OORVError::from(Diagnostic::error(&format!(
            "cannot read '{}': {}",
            path.display(),
            e
        )))
    })?;
    let spec_text = raw.trim().to_string();
    if spec_text.is_empty() {
        return Err(OORVError::from(Diagnostic::error(
            "specification file is empty",
        )));
    }
    compile_refined_text(spec_text, path.display().to_string())
}
