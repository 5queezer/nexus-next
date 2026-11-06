pub const SCOPE_TOOLS: &str = "mcp:tools";
pub const SCOPE_SUBMISSIONS: &str = "mcp:submissions";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthType {
    OAuth,
    ApiToken,
}

/// Who is calling. There is a single owner, so this only carries how they authenticated and what
/// they may do. The auth layer inserts it into request extensions; tool handlers read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthContext {
    pub auth_type: AuthType,
    pub scopes: Vec<String>,
}

impl AuthContext {
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}
