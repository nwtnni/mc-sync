use std::collections::HashSet;
use std::sync::Arc;

use joinery::JoinableIterator;
use once_cell::sync::Lazy;
use regex::Regex;
use serenity::client;
use serenity::framework;
use serenity::model::channel;
use serenity::model::id;
use structopt::StructOpt;
use tokio::io;
use tokio::io::AsyncBufReadExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio::process;
use tokio::sync::mpsc;

#[derive(Debug, StructOpt)]
struct Opt {
    #[structopt(short, long, env)]
    discord_token: String,

    #[structopt(short, long, env)]
    general_channel: u64,

    #[structopt(short, long, env)]
    server_channel: u64,

    command: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let opt = Opt::from_args();

    let (tx, mut rx) = mpsc::channel(10);

    let (mut child_stdin, minecraft) = Minecraft::new(&opt.command, tx.clone());
    let (mut stdout, stdin) = Stdin::new(tx.clone());
    let mut discord = serenity::Client::builder(&opt.discord_token)
        .event_handler(Discord(tx))
        .framework(framework::StandardFramework::default())
        .await?;

    let http = Arc::clone(&discord.cache_and_http);
    let general_channel = id::ChannelId::from(opt.general_channel);
    let server_channel = id::ChannelId::from(opt.server_channel);
    let mut online = HashSet::<String>::new();

    let minecraft = tokio::spawn(async move { minecraft.start().await });
    let stdin = tokio::spawn(async move { stdin.start().await });
    let discord = tokio::spawn(async move {
        // Might disconnect on hibernation.
        loop {
            discord.start().await.ok();
        }
    });

    let main = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                Event::Discord(message) => {
                    if message.author.name == "mc-sync" {
                        continue;
                    }

                    if message.content.trim() == "!online" {
                        let online =
                            format!("{} online: {}", online.len(), online.iter().join_with(", "));
                        message
                            .channel_id
                            .send_message(&http.http, |builder| builder.content(online))
                            .await?;
                        continue;
                    }

                    let say = format!("/say [{}]: {}\n", message.author.name, message.content);
                    child_stdin.write_all(say.as_bytes()).await?;
                    child_stdin.flush().await?;
                }
                Event::Minecraft(message) => {
                    stdout.write_all(message.as_bytes()).await?;
                    stdout.write_all(&[b'\n']).await?;
                    stdout.flush().await?;

                    server_channel
                        .send_message(&http.http, |builder| builder.content(&message))
                        .await?;

                    let message = if let Some(captures) = JOIN.captures(&message) {
                        online.insert(captures[1].to_owned());
                        format!("{} joined the server!", &captures[1])
                    } else if let Some(captures) = QUIT.captures(&message) {
                        online.remove(&captures[1]);
                        format!("{} left the server.", &captures[1])
                    } else if let Some(captures) = ACHIEVEMENT.captures(&message) {
                        format!("{} unlocked achievement [{}]!", &captures[1], &captures[2])
                    } else if let Some(captures) = MESSAGE.captures(&message) {
                        format!("[{}]: {}", &captures[1], &captures[2])
                    } else {
                        continue;
                    };

                    general_channel
                        .send_message(&http.http, |builder| builder.content(&message))
                        .await?;
                }
                Event::Stdin(mut message) => {
                    message.push('\n');
                    child_stdin.write_all(message.as_bytes()).await?;
                    child_stdin.flush().await?;
                }
            }
        }
        Result::<_, anyhow::Error>::Ok(())
    });

    tokio::select! {
        result = discord => result?,
        result = minecraft => result??,
        result = stdin => result??,
        result = main => result??,
    }

    Ok(())
}

#[derive(Clone, Debug)]
enum Event {
    Discord(channel::Message),
    Minecraft(String),
    Stdin(String),
}

struct Discord(mpsc::Sender<Event>);

#[serenity::async_trait]
impl client::EventHandler for Discord {
    async fn message(&self, _: client::Context, message: channel::Message) {
        self.0
            .send(Event::Discord(message))
            .await
            .expect("[INTERNAL ERROR]: `rx` dropped");
    }
}

static JOIN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r".*\[Server thread/INFO\]: (.*)\[[^\]]*\] logged in with entity id .* at .*")
        .unwrap()
});

static QUIT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r".*\[Server thread/INFO\]: (.*) left the game").unwrap());

static ACHIEVEMENT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r".*\[Server thread/INFO\]: (.*) has made the advancement \[(.*)\]").unwrap()
});

static MESSAGE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r".*\[Server thread/INFO\]: <([^ \]]*)> (.*)").unwrap());

struct Minecraft {
    #[allow(unused)]
    child: process::Child,
    stdout: io::BufReader<process::ChildStdout>,
    tx: mpsc::Sender<Event>,
}

impl Minecraft {
    fn new(command: &str, tx: mpsc::Sender<Event>) -> (io::BufWriter<process::ChildStdin>, Self) {
        let mut child = process::Command::new(command)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("Failed to launch server");
        let stdout = child
            .stdout
            .take()
            .map(io::BufReader::new)
            .expect("[IMPOSSIBLE]: stdout is piped");
        let stdin = child
            .stdin
            .take()
            .map(io::BufWriter::new)
            .expect("[IMPOSSIBLE]: stdin is piped");
        (stdin, Minecraft { child, stdout, tx })
    }

    async fn start(self) -> anyhow::Result<()> {
        let mut lines = self.stdout.lines();
        while let Some(line) = lines.next_line().await? {
            self.tx.send(Event::Minecraft(line)).await?;
        }
        Ok(())
    }
}

struct Stdin {
    stdin: io::BufReader<io::Stdin>,
    tx: mpsc::Sender<Event>,
}

impl Stdin {
    fn new(tx: mpsc::Sender<Event>) -> (io::BufWriter<io::Stdout>, Self) {
        let stdin = io::BufReader::new(io::stdin());
        let stdout = io::BufWriter::new(io::stdout());
        (stdout, Stdin { stdin, tx })
    }

    async fn start(self) -> anyhow::Result<()> {
        let mut lines = self.stdin.lines();
        while let Some(line) = lines.next_line().await? {
            self.tx.send(Event::Stdin(line)).await?;
        }
        Ok(())
    }
}
