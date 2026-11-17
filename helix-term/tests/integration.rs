#[cfg(feature = "integration")]
mod test {
    mod helpers;

    use helix_core::{syntax::config::AutoPairConfig, Selection};
    use helix_term::config::Config;

    use indoc::indoc;

    use self::helpers::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn hello_world() -> anyhow::Result<()> {
        test(("#[\n|]#", "ihello world<esc>", "hello world#[|\n]#")).await?;
        Ok(())
    }

    mod auto_pairs;
    #[cfg(unix)]
    mod bridge;
    #[cfg(unix)]
    mod bridge_annotations;
    mod command_line;
    mod commands;
    #[cfg(unix)]
    #[path = "../../../bb-bridge/bench/keypress.rs"]
    mod keypress_bench;
    mod movement;
    mod splits;
}
