use clap::CommandFactory;

use crate::cli::{Cli, CompletionsArgs};

pub fn run(args: &CompletionsArgs) -> anyhow::Result<()> {
    let mut cmd = Cli::command();
    clap_complete::generate(args.shell, &mut cmd, "urd", &mut std::io::stdout());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap_complete::Shell;

    fn generate_to_string(shell: Shell) -> String {
        let mut cmd = Cli::command();
        let mut buf = Vec::new();
        clap_complete::generate(shell, &mut cmd, "urd", &mut buf);
        String::from_utf8(buf).expect("completions should be valid UTF-8")
    }

    #[test]
    fn completions_bash_nonempty() {
        let output = generate_to_string(Shell::Bash);
        assert!(!output.is_empty(), "bash completions should not be empty");
    }

    #[test]
    fn completions_zsh_nonempty() {
        let output = generate_to_string(Shell::Zsh);
        assert!(!output.is_empty(), "zsh completions should not be empty");
    }

    #[test]
    fn completions_contains_subcommands() {
        let output = generate_to_string(Shell::Bash);
        assert!(output.contains("status"), "completions should contain 'status'");
        assert!(output.contains("backup"), "completions should contain 'backup'");
        assert!(output.contains("plan"), "completions should contain 'plan'");
    }

    #[test]
    fn completions_works_without_config() {
        // Completions must work without any config file — they only need the CLI definition.
        // This test verifies the function succeeds without touching Config::load().
        let args = CompletionsArgs { shell: Shell::Bash };
        // run() writes to stdout which we can't easily capture in a unit test,
        // but we can verify it doesn't error. The generate_to_string tests above
        // verify the actual output content.
        assert!(run(&args).is_ok(), "completions should work without config");
    }
}
