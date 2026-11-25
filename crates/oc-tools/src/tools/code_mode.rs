//! The built-in tools under `experimental.full_codemode` (2.3.1): `shell`, `read`, `edit`, `write`,
//! `patch` and `websearch` move into Code Mode, where programs call them as `tools.<name>`, and `grep`, `glob` and
//! `webfetch` are hidden (programs use their own code and the `fetch` global). Each plugin registers its tool with
//! the Code Mode options, so the change is made here, over the registered tool (#64284, #64335, #64364, #64368,
//! #64372, #64386, #64475, #64497).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::tool::{DefinitionContext, Tool, ToolContext, ToolDefinition, ToolError, ToolOptions, ToolResult};

/// The tools a location without them registers instead of hiding (`if (fullCodeMode) return`).
pub const HIDDEN: &[&str] = &["glob", "grep", "webfetch"];

/// `read`'s description in Code Mode, which attaches media to the `execute` result instead of numbering lines.
pub const READ_DESCRIPTION: &str = "Read the contents of a file or directory. Supports text files, images, and PDFs. Reading an image or PDF automatically attaches it to the execute result, even if the program doesn't return or print the media reference. Use offset and limit to read large files or directories in sections. Prefer one larger read over many small slices, and use grep to find specific content in large files.";

/// The Code Mode registration of a built-in tool, or `None` for a tool that stays direct.
pub fn variant(tool: &Arc<dyn Tool>) -> Option<Arc<dyn Tool>> {
    // Registered by another plugin under a built-in's name: left alone.
    if tool.options().namespace.is_some() {
        return None;
    }
    let pinned = match tool.name() {
        "shell" | "read" | "edit" | "write" | "patch" => true,
        "websearch" => false,
        _ => return None,
    };
    Some(Arc::new(CodeModeTool { inner: tool.clone(), pinned }))
}

struct CodeModeTool {
    inner: Arc<dyn Tool>,
    pinned: bool,
}

#[async_trait]
impl Tool for CodeModeTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn options(&self) -> ToolOptions {
        ToolOptions { codemode: true, pinned: self.pinned, ..self.inner.options() }
    }

    fn definition(&self, ctx: &DefinitionContext) -> ToolDefinition {
        match self.inner.name() {
            "shell" => super::shell::code_mode_definition(),
            "read" => ToolDefinition { description: READ_DESCRIPTION.into(), ..self.inner.definition(ctx) },
            _ => self.inner.definition(ctx),
        }
    }

    fn advertised(&self, ctx: &DefinitionContext) -> bool {
        self.inner.advertised(ctx)
    }

    fn prepare_input(&self, input: Value) -> Value {
        self.inner.prepare_input(input)
    }

    async fn execute(&self, mut input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        if self.inner.name() == "shell"
            && let Some(object) = input.as_object_mut()
        {
            // Code Mode input has no background parameter: decoding drops the key, so the command runs in the
            // foreground.
            object.shift_remove("background");
            // A Code Mode shell waits up to 10 minutes by default (`input.timeout || CODE_MODE_TIMEOUT_MS`).
            if object.get("timeout").and_then(Value::as_f64).is_none_or(|timeout| timeout == 0.0) {
                object.insert("timeout".into(), json!(super::shell::CODE_MODE_TIMEOUT_MS));
            }
        }
        self.inner.execute(input, ctx).await
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn shell_definition_has_no_background_and_the_long_default() {
        let definition = super::super::shell::code_mode_definition();
        let properties = definition.input_schema["properties"].as_object().unwrap();
        assert_eq!(properties.keys().collect::<Vec<_>>(), ["command", "workdir", "timeout"]);
        assert!(properties["timeout"]["description"].as_str().unwrap().contains("(default 600000)"));
        assert!(!definition.description.contains("Background commands return immediately"));
        assert!(definition.description.ends_with(
            "Commands still running after their timeout move to the background. The current session ID is available in the OPENCODE_SESSION_ID environment variable."
        ));
        let output = definition.output_schema.unwrap();
        assert!(output["properties"]["shellID"]["description"].as_str().unwrap().contains("A result without shellID is final"));
    }
}
