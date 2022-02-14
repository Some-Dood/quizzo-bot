mod error;

use crate::quiz::Quiz;
use error::{Error, Result};
use std::{collections::HashMap, sync::Arc, time::Duration};

use hyper::body::{self, Buf};
use hyper_trust_dns::RustlsHttpsConnector;
use parking_lot::RwLock;
use slab::Slab;
use tokio::{sync::mpsc, time};

use twilight_model::{
    application::{
        callback::{CallbackData, InteractionResponse},
        component::{select_menu::SelectMenuOption, ActionRow, Component, SelectMenu},
        interaction::{
            application_command::{CommandDataOption, CommandOptionValue},
            ApplicationCommand, Interaction, MessageComponentInteraction,
        },
    },
    channel::message::{AllowedMentions, MessageFlags},
    id::{
        marker::{ApplicationMarker, UserMarker},
        Id,
    },
};

type Event = (Id<UserMarker>, usize);
type Channel = mpsc::UnboundedSender<Event>;
type QuizRegistry = RwLock<Slab<Channel>>;

#[derive(Clone)]
pub struct Lobby {
    /// Container for all pending polls.
    quizzes: Arc<QuizRegistry>,
    /// Discord API interactions.
    api: Arc<twilight_http::Client>,
    /// Arbitrary HTTP fetching of JSON files.
    http: hyper::Client<RustlsHttpsConnector>,
    /// Application ID to match on.
    app: Id<ApplicationMarker>,
}

impl Lobby {
    const CREATE_NAME: &'static str = "create";
    const PARAM_NAME: &'static str = "url";

    pub fn new(token: String, app: Id<ApplicationMarker>) -> Self {
        // Initialize Discord API client
        let api = Arc::new(twilight_http::Client::new(token));

        // Initialize HTTP client for fetching JSON
        let connector = hyper_trust_dns::new_rustls_native_https_connector();
        let http = hyper::Client::builder().http2_only(true).build(connector);

        Self {
            app,
            api,
            http,
            quizzes: Arc::default(),
        }
    }

    pub async fn on_interaction(&self, interaction: Interaction) -> InteractionResponse {
        let result = match interaction {
            Interaction::Ping(_) => Ok(InteractionResponse::Pong),
            Interaction::ApplicationCommand(comm) => self.on_app_comm(*comm).await,
            Interaction::MessageComponent(msg) => self.on_msg_interaction(*msg).await,
            _ => Err(Error::UnsupportedInteraction),
        };

        let text = match result {
            Ok(res) => return res,
            Err(err) => err.to_string(),
        };

        InteractionResponse::ChannelMessageWithSource(CallbackData {
            content: Some(text),
            flags: Some(MessageFlags::EPHEMERAL),
            tts: None,
            allowed_mentions: None,
            components: None,
            embeds: None,
        })
    }

    /// Responds to new application commands.
    async fn on_app_comm(&self, mut comm: ApplicationCommand) -> Result<InteractionResponse> {
        if comm.data.name.as_str() != Self::CREATE_NAME {
            return Err(Error::UnknownCommandName);
        }

        // NOTE: We pop off the values to attain O(1) removal time.
        // This does mean that the validation will fail if there are more
        // than one arguments supplied. This should be alright for now since
        // we don't expect the `create` command to accept more than one argument.
        let (name, value) = match comm.data.options.pop() {
            Some(CommandDataOption {
                name,
                value: CommandOptionValue::String(value),
                ..
            }) => (name, value),
            _ => return Err(Error::InvalidParams),
        };

        if name.as_str() != Self::PARAM_NAME {
            return Err(Error::UnknownParamName);
        }

        drop(name);
        let uri = value.parse().map_err(|_| Error::InvalidUri)?;
        drop(value);

        let body = self.http.get(uri).await.map_err(|_| Error::FailedFetch)?.into_body();
        let buf = body::aggregate(body).await?.reader();
        let Quiz {
            question,
            choices,
            timeout,
            answer,
        } = serde_json::from_reader(buf)?;
        let answer = usize::from(answer);
        let correct = choices.get(answer).ok_or(Error::Data)?.clone().into_boxed_str();

        // Open channel to receiving new answers
        let (tx, mut rx) = mpsc::unbounded_channel();
        let quiz_id = self.quizzes.write().insert(tx);

        // Spawn external Tokio task for handling incoming responses
        let api = Arc::clone(&self.api);
        let quizzes = Arc::clone(&self.quizzes);
        let app_id = self.app;
        tokio::spawn(async move {
            // Keep processing new answers
            let mut selections = HashMap::new();
            let timer = time::sleep(Duration::from_secs(timeout.into()));
            tokio::pin!(timer);
            loop {
                tokio::select! {
                    biased;
                    Some((user, choice)) = rx.recv() => selections.insert(user, choice),
                    _ = &mut timer => break,
                    else => unreachable!(),
                };
            }

            // Disable all communication channels
            drop(rx);
            quizzes.write().remove(quiz_id);
            drop(quizzes);

            // Finalize the poll
            let winners: Vec<_> = selections
                .into_iter()
                .filter_map(|(user, choice)| if choice == answer { Some(user) } else { None })
                .collect();
            let content = if winners.is_empty() {
                format!("The correct answer is: **{correct}**. Nobody got it right...")
            } else {
                let mentions: Vec<_> = winners.iter().copied().map(|id| format!("<@{id}>")).collect();
                let congrats = mentions.join(" ");
                format!("The correct answer is: **{correct}**. Congratulations to {congrats}!")
            };
            api.interaction(app_id)
                .create_followup_message(&comm.token)
                .content(&content)
                .unwrap()
                .allowed_mentions(&AllowedMentions {
                    users: winners,
                    ..Default::default()
                })
                .exec()
                .await
                .unwrap();
        });

        let options = choices
            .into_iter()
            .enumerate()
            .map(|(i, label)| SelectMenuOption {
                label,
                description: None,
                emoji: None,
                default: false,
                value: i.to_string(),
            })
            .collect();
        let comps = Vec::from([Component::ActionRow(ActionRow {
            components: Vec::from([Component::SelectMenu(SelectMenu {
                options,
                custom_id: quiz_id.to_string(),
                placeholder: Some(String::from("Your Selection")),
                disabled: false,
                min_values: Some(1),
                max_values: Some(1),
            })]),
        })]);
        Ok(InteractionResponse::ChannelMessageWithSource(CallbackData {
            content: Some(question),
            components: Some(comps),
            flags: None,
            tts: None,
            allowed_mentions: None,
            embeds: None,
        }))
    }

    /// Responds to message component interactions.
    async fn on_msg_interaction(&self, mut msg: MessageComponentInteraction) -> Result<InteractionResponse> {
        let quiz_id = msg.data.custom_id.parse().map_err(|_| Error::Unrecoverable)?;
        let user = msg
            .member
            .and_then(|m| m.user)
            .or(msg.user)
            .ok_or(Error::UnknownUser)?
            .id;

        // Since we know that there can only be one value from this interaction,
        // we simply pop the arguments directly. This allows O(1) deletion.
        let arg = msg.data.values.pop().ok_or(Error::Unrecoverable)?;
        let choice = arg.parse().map_err(|_| Error::Data)?;
        drop(arg);

        self.quizzes
            .read()
            .get(quiz_id)
            .ok_or(Error::Unrecoverable)?
            .send((user, choice))
            .map_err(|_| Error::Unrecoverable)?;

        Ok(InteractionResponse::ChannelMessageWithSource(CallbackData {
            content: Some(String::from("We have received your selection.")),
            flags: Some(MessageFlags::EPHEMERAL),
            components: None,
            tts: None,
            allowed_mentions: None,
            embeds: None,
        }))
    }
}
