use miden_client::Client;
use miden_client::note::NoteTag;
use tracing::info;

use crate::errors::CliError;
use crate::{Parser, create_dynamic_table};

#[derive(Default, Debug, Parser, Clone)]
#[command(about = "View and manage tags. Defaults to `list` command")]
pub struct TagsCmd {
    /// List all tags monitored by this client.
    #[arg(short, long, group = "action")]
    list: bool,

    /// Add a new tag to the list of tags monitored by this client.
    #[arg(short, long, group = "action", value_name = "tag")]
    add: Option<u32>,

    /// Removes a tag from the list of tags monitored by this client.
    #[arg(short, long, group = "action", value_name = "tag")]
    remove: Option<u32>,
}

impl TagsCmd {
    pub async fn execute<AUTH>(&self, client: Client<AUTH>) -> Result<(), CliError> {
        match self {
            TagsCmd { add: Some(tag), .. } => {
                add_tag(client, *tag).await?;
            },
            TagsCmd { remove: Some(tag), .. } => {
                remove_tag(client, *tag).await?;
            },
            _ => {
                list_tags(client).await?;
            },
        }
        Ok(())
    }
}

// HELPERS
// ================================================================================================
async fn list_tags<AUTH>(client: Client<AUTH>) -> Result<(), CliError> {
    let mut table = create_dynamic_table(&["Tag", "Source"]);

    let tags = client.get_note_tags().await?;

    for tag in tags {
        let source = match tag.source {
            miden_client::sync::NoteTagSource::Account(account_id) => {
                format!("Account({})", account_id.to_hex())
            },
            miden_client::sync::NoteTagSource::Note(details_commitment) => {
                format!("Note({})", details_commitment.to_hex())
            },
            miden_client::sync::NoteTagSource::User => "User".to_string(),
            miden_client::sync::NoteTagSource::Subscription(key) => {
                format!("Subscription({})", key.to_hex())
            },
        };

        table.add_row(vec![tag.tag.to_string(), source]);
    }

    println!("\n{table}");

    Ok(())
}

async fn add_tag<AUTH>(mut client: Client<AUTH>, tag: u32) -> Result<(), CliError> {
    let tag: NoteTag = tag.into();
    info!("adding tag {tag}");
    if client.add_note_tag(tag).await? {
        println!("Tag {tag} added");
    } else {
        println!("Tag {tag} is already being tracked");
    }
    Ok(())
}

async fn remove_tag<AUTH>(mut client: Client<AUTH>, tag: u32) -> Result<(), CliError> {
    let tag: NoteTag = tag.into();
    if client.remove_note_tag(tag).await? {
        println!("Tag {tag} removed");
    } else {
        println!("Tag {tag} wasn't being tracked");
    }
    Ok(())
}
