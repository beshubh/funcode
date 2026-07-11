use crate::app::{App, AppAction};
use std::{fmt, sync::Arc};

pub trait Command: fmt::Debug + Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn execute(&self, _app: &mut App) -> Option<AppAction> {
        None
    }
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
        registry.register(ModelsCommand);
        registry.register(ThemeCommand);
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

#[derive(Debug)]
struct ModelsCommand;

impl Command for ModelsCommand {
    fn name(&self) -> &'static str {
        "models"
    }

    fn description(&self) -> &'static str {
        "Choose a model from supported providers"
    }

    fn execute(&self, app: &mut App) -> Option<AppAction> {
        app.open_models_dialog();
        Some(AppAction::ListModels)
    }
}

#[derive(Debug)]
struct ThemeCommand;

impl Command for ThemeCommand {
    fn name(&self) -> &'static str {
        "theme"
    }

    fn description(&self) -> &'static str {
        "Choose a color theme"
    }

    fn execute(&self, app: &mut App) -> Option<AppAction> {
        app.open_theme_dialog();
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, CommandRegistry};
    use crate::app::{App, AppAction};

    #[derive(Debug)]
    struct TestCommand {
        name: &'static str,
    }

    impl Command for TestCommand {
        fn name(&self) -> &'static str {
            self.name
        }

        fn description(&self) -> &'static str {
            "Test command"
        }

        fn execute(&self, _app: &mut App) -> Option<AppAction> {
            None
        }
    }

    #[test]
    fn matching_is_case_insensitive_and_preserves_registration_order() {
        let mut registry = CommandRegistry::empty();
        registry.register(TestCommand { name: "auth" });
        registry.register(TestCommand { name: "author" });
        registry.register(TestCommand { name: "exit" });

        let names: Vec<_> = registry
            .matching("AU")
            .map(|command| command.name())
            .collect();

        assert_eq!(names, ["auth", "author"]);
    }

    #[test]
    fn find_requires_an_exact_command_name() {
        let mut registry = CommandRegistry::empty();
        registry.register(TestCommand { name: "auth" });

        assert_eq!(
            registry.find("auth").map(|command| command.name()),
            Some("auth")
        );
        assert!(registry.find("Auth").is_none());
        assert!(registry.find("au").is_none());
    }
}
