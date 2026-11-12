#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Pass(String),
    Nick(String),
    User(String),
    Join(String),
    Part(String),
    Privmsg {
        target: String,
        body: String,
    },
    Ping(String),
    Pong,
    Quit,
    Cap {
        subcommand: String,
        capabilities: Vec<String>,
    },
    History {
        channel: Option<String>,
        after_seq: i64,
        limit: i64,
    },
    TaskCreate {
        channel: String,
        title: String,
    },
    TaskClaim(String),
    TaskDone(String),
    TaskRelease(String),
    TaskList(String),
    Names(Option<String>),
    Who(Option<String>),
    RoleSet {
        nick: String,
        role: String,
    },
    RoleList,
    LockAcquire {
        resource: String,
        channel: Option<String>,
        ttl_seconds: Option<i64>,
        note: Option<String>,
    },
    LockRelease(String),
    LockList(Option<String>),
    Notice {
        target: String,
        body: String,
    },
    Mode {
        target: String,
        flags: Option<String>,
        arg: Option<String>,
    },
    Topic {
        channel: String,
        topic: Option<String>,
    },
    List(Option<String>),
    Whois(String),
    Ison(Vec<String>),
    Userhost(Vec<String>),
    Away(Option<String>),
    Unknown(String),
}

pub fn parse_command(line: &str) -> Command {
    let line = strip_tags_and_prefix(line.trim());
    let (head, trailing) = split_trailing(line);
    let mut parts = head.split_whitespace();
    let verb = parts.next().unwrap_or_default().to_ascii_uppercase();

    match verb.as_str() {
        "PASS" => Command::Pass(parts.collect::<Vec<_>>().join(" ")),
        "NICK" => Command::Nick(parts.next().unwrap_or_default().to_string()),
        "USER" => Command::User(parts.next().unwrap_or_default().to_string()),
        "JOIN" => Command::Join(parts.next().unwrap_or_default().to_string()),
        "PART" => Command::Part(parts.next().unwrap_or_default().to_string()),
        "PRIVMSG" => Command::Privmsg {
            target: parts.next().unwrap_or_default().to_string(),
            body: trailing.unwrap_or_else(|| parts.collect::<Vec<_>>().join(" ")),
        },
        "PING" => Command::Ping(trailing.unwrap_or_else(|| parts.collect::<Vec<_>>().join(" "))),
        "PONG" => Command::Pong,
        "QUIT" => Command::Quit,
        "CAP" => Command::Cap {
            subcommand: parts.next().unwrap_or_default().to_ascii_uppercase(),
            capabilities: trailing
                .unwrap_or_else(|| parts.collect::<Vec<_>>().join(" "))
                .split_whitespace()
                .map(str::to_string)
                .collect(),
        },
        "CHATHISTORY" => parse_chathistory(parts.collect::<Vec<_>>()),
        "HISTORY" => Command::History {
            channel: Some(parts.next().unwrap_or_default().to_string()),
            after_seq: parts.next().unwrap_or("0").parse().unwrap_or(0),
            limit: parts.next().unwrap_or("100").parse().unwrap_or(100),
        },
        "TASK" => parse_task(parts.collect::<Vec<_>>(), trailing),
        "NAMES" => Command::Names(parts.next().map(str::to_string).filter(|s| !s.is_empty())),
        "WHO" => Command::Who(parts.next().map(str::to_string).filter(|s| !s.is_empty())),
        "ROLES" => Command::RoleList,
        "ROLE" => parse_role(parts.collect::<Vec<_>>()),
        "LOCKS" => Command::LockList(parts.next().map(str::to_string).filter(|s| !s.is_empty())),
        "LOCK" => parse_lock(parts.collect::<Vec<_>>(), trailing),
        "UNLOCK" => Command::LockRelease(parts.next().unwrap_or_default().to_string()),
        "NOTICE" => Command::Notice {
            target: parts.next().unwrap_or_default().to_string(),
            body: trailing.unwrap_or_else(|| parts.collect::<Vec<_>>().join(" ")),
        },
        "MODE" => Command::Mode {
            target: parts.next().unwrap_or_default().to_string(),
            flags: parts.next().map(str::to_string).filter(|s| !s.is_empty()),
            arg: parts
                .next()
                .map(str::to_string)
                .filter(|s| !s.is_empty())
                .or(trailing),
        },
        "TOPIC" => Command::Topic {
            channel: parts.next().unwrap_or_default().to_string(),
            topic: trailing,
        },
        "LIST" => Command::List(parts.next().map(str::to_string).filter(|s| !s.is_empty())),
        "WHOIS" => Command::Whois(parts.next().unwrap_or_default().to_string()),
        "ISON" => Command::Ison(
            trailing
                .unwrap_or_else(|| parts.collect::<Vec<_>>().join(" "))
                .split_whitespace()
                .map(str::to_string)
                .collect(),
        ),
        "USERHOST" => Command::Userhost(parts.map(str::to_string).collect()),
        "AWAY" => Command::Away(trailing.filter(|s| !s.is_empty())),
        _ => Command::Unknown(verb),
    }
}

fn parse_chathistory(parts: Vec<&str>) -> Command {
    if parts
        .first()
        .is_some_and(|part| part.eq_ignore_ascii_case("AFTER"))
    {
        if parts.get(1).is_some_and(|value| value.starts_with('#')) {
            return Command::History {
                channel: parts.get(1).map(|value| value.to_string()),
                after_seq: parts.get(2).copied().unwrap_or("0").parse().unwrap_or(0),
                limit: parts
                    .get(3)
                    .copied()
                    .unwrap_or("100")
                    .parse()
                    .unwrap_or(100),
            };
        }

        return Command::History {
            channel: None,
            after_seq: parts.get(1).copied().unwrap_or("0").parse().unwrap_or(0),
            limit: parts
                .get(2)
                .copied()
                .unwrap_or("100")
                .parse()
                .unwrap_or(100),
        };
    }

    Command::History {
        channel: parts.first().map(|value| value.to_string()),
        after_seq: parts.get(1).copied().unwrap_or("0").parse().unwrap_or(0),
        limit: parts
            .get(2)
            .copied()
            .unwrap_or("100")
            .parse()
            .unwrap_or(100),
    }
}

fn parse_task(parts: Vec<&str>, trailing: Option<String>) -> Command {
    let Some(action) = parts.first().map(|part| part.to_ascii_uppercase()) else {
        return Command::Unknown("TASK".to_string());
    };

    match action.as_str() {
        "CREATE" => Command::TaskCreate {
            channel: parts.get(1).copied().unwrap_or_default().to_string(),
            title: trailing
                .unwrap_or_else(|| parts.iter().skip(2).copied().collect::<Vec<_>>().join(" ")),
        },
        "CLAIM" => Command::TaskClaim(parts.get(1).copied().unwrap_or_default().to_string()),
        "DONE" => Command::TaskDone(parts.get(1).copied().unwrap_or_default().to_string()),
        "RELEASE" => Command::TaskRelease(parts.get(1).copied().unwrap_or_default().to_string()),
        "LIST" => Command::TaskList(parts.get(1).copied().unwrap_or_default().to_string()),
        _ => Command::Unknown(format!("TASK {action}")),
    }
}

fn parse_role(parts: Vec<&str>) -> Command {
    match (parts.first().copied(), parts.get(1).copied()) {
        (None, _) => Command::RoleList,
        (Some(nick), Some(role)) => Command::RoleSet {
            nick: nick.to_string(),
            role: role.to_string(),
        },
        (Some(only), None) => {
            if only.eq_ignore_ascii_case("LIST") {
                Command::RoleList
            } else {
                Command::Unknown("ROLE".to_string())
            }
        }
    }
}

fn parse_lock(parts: Vec<&str>, trailing: Option<String>) -> Command {
    let Some(&resource) = parts.first() else {
        return Command::LockList(None);
    };
    if resource.eq_ignore_ascii_case("LIST") && parts.len() <= 2 && trailing.is_none() {
        return Command::LockList(parts.get(1).map(|s| s.to_string()));
    }

    let mut idx = 1;
    let mut channel = None;
    let mut ttl_seconds = None;

    if parts
        .get(idx)
        .is_some_and(|p| p.starts_with('#') || p.starts_with('&'))
    {
        channel = Some(parts[idx].to_string());
        idx += 1;
    }

    if let Some(Ok(ttl)) = parts.get(idx).map(|p| p.parse::<i64>()) {
        ttl_seconds = Some(ttl);
        idx += 1;
    }

    let note = trailing.or_else(|| {
        if idx < parts.len() {
            Some(parts[idx..].join(" "))
        } else {
            None
        }
    });

    Command::LockAcquire {
        resource: resource.to_string(),
        channel,
        ttl_seconds,
        note,
    }
}

fn strip_tags_and_prefix(line: &str) -> &str {
    let without_tags = if line.starts_with('@') {
        line.split_once(' ').map(|(_, rest)| rest).unwrap_or(line)
    } else {
        line
    };

    if without_tags.starts_with(':') {
        without_tags
            .split_once(' ')
            .map(|(_, rest)| rest)
            .unwrap_or(without_tags)
    } else {
        without_tags
    }
}

fn split_trailing(line: &str) -> (&str, Option<String>) {
    if let Some((head, trailing)) = line.split_once(" :") {
        (head, Some(trailing.to_string()))
    } else {
        (line, None)
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_command, Command};

    #[test]
    fn parses_privmsg_with_trailing_body() {
        assert_eq!(
            parse_command("PRIVMSG #demo :hello world"),
            Command::Privmsg {
                target: "#demo".to_string(),
                body: "hello world".to_string()
            }
        );
    }

    #[test]
    fn parses_task_create() {
        assert_eq!(
            parse_command("TASK CREATE #demo :fix flaky test"),
            Command::TaskCreate {
                channel: "#demo".to_string(),
                title: "fix flaky test".to_string()
            }
        );
    }

    #[test]
    fn parses_chathistory_after() {
        assert_eq!(
            parse_command("CHATHISTORY AFTER 42 100"),
            Command::History {
                channel: None,
                after_seq: 42,
                limit: 100
            }
        );
    }

    #[test]
    fn parses_chathistory_after_channel() {
        assert_eq!(
            parse_command("CHATHISTORY AFTER #demo 42 100"),
            Command::History {
                channel: Some("#demo".to_string()),
                after_seq: 42,
                limit: 100
            }
        );
    }

    #[test]
    fn parses_cap_req() {
        assert_eq!(
            parse_command("CAP REQ :message-tags"),
            Command::Cap {
                subcommand: "REQ".to_string(),
                capabilities: vec!["message-tags".to_string()]
            }
        );
    }

    #[test]
    fn parses_roles_and_locks() {
        assert_eq!(
            parse_command("ROLE agent-a reviewer"),
            Command::RoleSet {
                nick: "agent-a".to_string(),
                role: "reviewer".to_string(),
            }
        );
        assert_eq!(parse_command("ROLES"), Command::RoleList);
        assert_eq!(
            parse_command("LOCK quest-1 #vr-lab 600 :testing tracking"),
            Command::LockAcquire {
                resource: "quest-1".to_string(),
                channel: Some("#vr-lab".to_string()),
                ttl_seconds: Some(600),
                note: Some("testing tracking".to_string()),
            }
        );
        assert_eq!(
            parse_command("LOCKS #vr-lab"),
            Command::LockList(Some("#vr-lab".to_string()))
        );
        assert_eq!(
            parse_command("UNLOCK quest-1"),
            Command::LockRelease("quest-1".to_string())
        );
        assert_eq!(
            parse_command("MODE #general +o agent-a"),
            Command::Mode {
                target: "#general".to_string(),
                flags: Some("+o".to_string()),
                arg: Some("agent-a".to_string()),
            }
        );
    }
}
