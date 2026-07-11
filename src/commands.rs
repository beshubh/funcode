use crate::app::{App, AppAction};
use std::{fmt, sync::Arc};

pub trait Command: fmt::Debug + Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn execute(&self, app: &mut App) -> Option<AppAction>;
}

pub struct CommandRegistry {
    commands: Vec<Arc<dyn Command>>,
}

impl fmt::Debug for CommandRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_list()
            .entries(self.commands.iter().map(|command| command.name()))
            .finish()
    }
}

impl CommandRegistry {
    pub fn with_builtins() -> Self {
        Self::default()
    }

    pub fn empty() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    pub fn register(&mut self, command: impl Command + 'static) {
        self.commands.push(Arc::new(command));
    }

    pub fn matching(&self, query: &str) -> impl Iterator<Item = Arc<dyn Command>> + '_ {
        let query = query.to_lowercase();
        self.commands
            .iter()
            .filter(move |command| command.name().to_lowercase().starts_with(&query))
            .cloned()
    }

    pub fn find(&self, name: &str) -> Option<Arc<dyn Command>> {
        self.commands
            .iter()
            .find(|command| command.name() == name)
            .cloned()
    }
}

impl Default for CommandRegistry {
    fn default() -> Self {
        let mut registry = Self::empty();
        registry.register(AuthCommand);
        registry.register(ExitCommand);
        registry
    }
}

#[derive(Debug)]
struct AuthCommand;

impl Command for AuthCommand {
    fn name(&self) -> &'static str {
        "auth"
    }

    fn description(&self) -> &'static str {
        "Authenticate with a provider"
    }

    fn execute(&self, app: &mut App) -> Option<AppAction> {
        app.open_auth_dialog();
        None
    }
}

#[derive(Debug)]
struct ExitCommand;

impl Command for ExitCommand {
    fn name(&self) -> &'static str {
        "exit"
    }

    fn description(&self) -> &'static str {
        "Exit funcode"
    }

    fn execute(&self, _app: &mut App) -> Option<AppAction> {
        Some(AppAction::Quit)
    }
}

#[cfg(test)]
mod tests {
    use super::CommandRegistry;

    #[test]
    fn builtins_can_be_matched_by_prefix_and_found_by_exact_name() {
        let registry = CommandRegistry::with_builtins();

        let matches: Vec<_> = registry
            .matching("a")
            .map(|command| command.name())
            .collect();

        assert_eq!(matches, ["auth"]);
        assert_eq!(registry.find("exit").unwrap().description(), "Exit funcode");
        assert!(registry.find("missing").is_none());
    }
}
