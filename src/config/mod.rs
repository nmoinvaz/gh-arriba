mod manifest;
mod project;
mod sync;

use anyhow::Result;

use crate::api::gists::GistClient;
use crate::cli::{ConfigArgs, ConfigCommand};
use crate::context::Ctx;
use crate::prompt;
use project::ProjectConfig;

pub fn run(ctx: &Ctx, args: ConfigArgs) -> Result<()> {
    let config = ProjectConfig::new(args.name, args.root, args.private)?;
    let client = GistClient::new(&ctx.client);

    let gist_id = client.find_gist(&config.gist_description())?;

    match args.command {
        Some(ConfigCommand::Add { files, platform }) => {
            sync::do_add(&client, &config, &files, platform, gist_id.as_deref())?;
        }
        Some(ConfigCommand::Remove { files, platform }) => {
            sync::do_remove(&client, &config, &files, platform, gist_id.as_deref())?;
        }
        Some(ConfigCommand::Cleanup) => {
            sync::do_cleanup(&client, &config, gist_id.as_deref())?;
        }
        Some(ConfigCommand::Delete) => {
            let prompt = format!("Delete the {} gist?", config.gist_description());
            if prompt::confirm(&prompt, ctx.yes)? {
                sync::do_delete(&client, gist_id.as_deref())?;
            }
        }
        Some(ConfigCommand::Open) => {
            sync::do_open(gist_id.as_deref())?;
        }
        Some(ConfigCommand::Sync { files }) => {
            sync::do_sync(&client, &config, &files, gist_id.as_deref())?;
        }
        Some(ConfigCommand::Status { files }) => {
            sync::do_status(&client, &config, &files, gist_id.as_deref())?;
        }
        Some(ConfigCommand::Restore { files }) => {
            sync::do_restore(&client, &config, &files, gist_id.as_deref())?;
        }
        None => {
            sync::do_sync(&client, &config, &[], gist_id.as_deref())?;
        }
    }

    Ok(())
}
