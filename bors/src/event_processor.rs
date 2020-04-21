use crate::{command::Command, graphql::GithubClient, state::PullRequestState, Config};
use futures::{channel::mpsc, lock::Mutex, sink::SinkExt, stream::StreamExt};
use github::{Event, EventType, NodeId};
use hotpot_db::HotPot;
use log::info;
use std::collections::HashMap;

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Request {
    Webhook { event: Event, delivery_id: String },
}

#[derive(Clone, Debug)]
pub struct EventProcessorSender {
    inner: mpsc::Sender<Request>,
}

impl EventProcessorSender {
    pub fn new(inner: mpsc::Sender<Request>) -> Self {
        Self { inner }
    }

    pub async fn webhook(
        &mut self,
        event: Event,
        delivery_id: String,
    ) -> Result<(), mpsc::SendError> {
        self.inner
            .send(Request::Webhook { event, delivery_id })
            .await
    }
}

#[async_trait::async_trait]
impl probot::Service for EventProcessorSender {
    fn name(&self) -> &'static str {
        "bors"
    }

    fn route(&self, _event_type: EventType) -> bool {
        true
    }

    async fn handle(&self, event: &Event, delivery_id: &str) {
        self.clone()
            .webhook(event.clone(), delivery_id.to_owned())
            .await
            .unwrap();
    }
}

#[derive(Debug)]
pub struct EventProcessor {
    config: Config,
    github: GithubClient,
    pulls: HashMap<u64, PullRequestState>,
    db: Mutex<HotPot>,
    requests_rx: mpsc::Receiver<Request>,
}

impl EventProcessor {
    pub fn new(config: Config) -> (EventProcessorSender, Self) {
        let (tx, rx) = mpsc::channel(1024);
        let github = GithubClient::new(&config.github_api_token);

        (
            EventProcessorSender::new(tx),
            Self {
                config,
                github,
                pulls: HashMap::new(),
                db: Mutex::new(HotPot::new()),
                requests_rx: rx,
            },
        )
    }

    pub async fn start(mut self) {
        self.synchronize().await;

        while let Some(request) = self.requests_rx.next().await {
            self.handle_request(request).await
        }
    }

    async fn handle_request(&self, request: Request) {
        use Request::*;
        match request {
            Webhook { event, delivery_id } => self.handle_webhook(event, delivery_id).await,
        }
    }

    async fn handle_webhook(&self, event: Event, delivery_id: String) {
        info!("Handling Webhook: {}", delivery_id);

        //TODO route on the request
        match &event {
            Event::PullRequest(_e) => {}
            Event::IssueComment(e) => {
                // Only process commands from newly created comments
                if e.action.is_created() && e.issue.is_pull_request() {
                    self.process_comment(e.issue.number, e.comment.body(), &e.comment.node_id)
                        .await
                }
            }
            Event::PullRequestReview(e) => {
                if e.action.is_submitted() {
                    self.process_comment(e.pull_request.number, e.review.body(), &e.review.node_id)
                        .await
                }
            }
            Event::PullRequestReviewComment(e) => {
                if e.action.is_created() {
                    self.process_comment(
                        e.pull_request.number,
                        e.comment.body(),
                        &e.comment.node_id,
                    )
                    .await
                }
            }
            // Unsupported Event
            _ => {}
        }
    }

    async fn process_comment(&self, issue_number: u64, comment: Option<&str>, node_id: &NodeId) {
        info!("comment: {:#?}", comment);
        match comment.and_then(Command::from_comment) {
            Some(Ok(_)) => {
                info!("Valid Command");

                self.github
                    .add_reaction(node_id, github::ReactionType::Rocket)
                    .await
                    // TODO handle or ignore error
                    .unwrap();
            }
            Some(Err(_)) => {
                info!("Invalid Command");
                self.github
                    .issues()
                    .create_comment(
                        self.config.repo().owner(),
                        self.config.repo().name(),
                        issue_number,
                        &format!("{}", Command::help()),
                    )
                    .await
                    // TODO handle or ignore error
                    .unwrap();
            }
            None => {
                info!("No command in comment");
            }
        }
    }

    async fn synchronize(&mut self) {
        info!("Synchronizing");

        // TODO: Handle error
        let pulls = self
            .github
            .open_pulls(self.config.repo().owner(), self.config.repo().name())
            .await
            .unwrap();
        info!("{} Open PullRequests", pulls.len());

        // TODO: Scrape the comments/Reviews of each PR to pull out reviewer/approval data

        self.pulls.clear();
        self.pulls
            .extend(pulls.into_iter().map(|pr| (pr.number, pr)));

        info!("Done Synchronizing");
    }
}