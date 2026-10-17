use crate::error::{PromoteError, git};

const RULES: &str = include_str!("../rules/secrets.yar");

pub struct SecretScreen {
    rules: yara_x::Rules,
}

impl SecretScreen {
    pub fn new() -> Result<Self, PromoteError> {
        let mut compiler = yara_x::Compiler::new();
        compiler.add_source(RULES).map_err(git)?;
        Ok(Self {
            rules: compiler.build(),
        })
    }

    pub fn scan(&self, bytes: &[u8]) -> Result<Vec<String>, PromoteError> {
        let mut scanner = yara_x::Scanner::new(&self.rules);
        let results = scanner.scan(bytes).map_err(git)?;
        Ok(results
            .matching_rules()
            .map(|rule| rule.identifier().to_owned())
            .collect())
    }
}
