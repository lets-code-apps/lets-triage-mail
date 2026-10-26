//! A tiny fake IMAP server for tests that need a connection but not Dovecot (which only runs
//! in CI). It speaks just enough IMAP: greeting, LOGIN, CAPABILITY, APPEND and LOGOUT.

/// How the fake server finishes an `APPEND`.
#[derive(Clone, Copy)]
pub(crate) enum AppendReply {
    /// `OK [APPENDUID 77 12]`.
    WithUid,
    /// Plain `OK`, as from a server without UIDPLUS.
    WithoutUid,
    /// `NO [TRYCREATE]`, as for a folder that does not exist (before any `+`).
    NoSuchFolder,
}

/// Starts the fake server (it serves one connection) and returns its port and the bytes it
/// received as the message. `reply` says how it finishes an `APPEND`.
pub(crate) async fn fake_server(
    reply: AppendReply,
) -> (u16, tokio::sync::oneshot::Receiver<Vec<u8>>) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sent, received) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        write.write_all(b"* OK fake ready\r\n").await.unwrap();
        let mut sent = Some(sent);
        let mut line = String::new();
        loop {
            line.clear();
            if read.read_line(&mut line).await.unwrap() == 0 {
                return;
            }
            let tag = line.split_whitespace().next().unwrap().to_owned();
            let command = line
                .split_whitespace()
                .nth(1)
                .unwrap_or("")
                .to_ascii_uppercase();
            let answer: String = match command.as_str() {
                "LOGIN" => format!("{tag} OK logged in\r\n"),
                "CAPABILITY" => {
                    format!("* CAPABILITY IMAP4rev1 UIDPLUS\r\n{tag} OK done\r\n")
                }
                "LOGOUT" => format!("* BYE\r\n{tag} OK bye\r\n"),
                "APPEND" if matches!(reply, AppendReply::NoSuchFolder) => {
                    format!("{tag} NO [TRYCREATE] no such folder\r\n")
                }
                "APPEND" => {
                    // `APPEND "INBOX" {123}`: the size is the number in braces.
                    let size: usize = line
                        .rsplit('{')
                        .next()
                        .and_then(|s| s.trim().trim_end_matches('}').parse().ok())
                        .expect("a literal size");
                    write.write_all(b"+ go ahead\r\n").await.unwrap();
                    let mut message = vec![0u8; size + 2];
                    read.read_exact(&mut message).await.unwrap();
                    assert_eq!(&message[size..], b"\r\n", "the command ends with CRLF");
                    message.truncate(size);
                    if let Some(sent) = sent.take() {
                        let _ = sent.send(message);
                    }
                    match reply {
                        AppendReply::WithUid => {
                            format!("* 3 EXISTS\r\n{tag} OK [APPENDUID 77 12] done\r\n")
                        }
                        _ => format!("{tag} OK done\r\n"),
                    }
                }
                other => panic!("the fake server got an unexpected command: {other}"),
            };
            write.write_all(answer.as_bytes()).await.unwrap();
        }
    });
    (port, received)
}
