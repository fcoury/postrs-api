//! IMAP backend module.
//!
//! This module contains the definition of the IMAP backend.

use imap::{extensions::idle::SetReadTimeout, types::NameAttribute};
use log::{debug, log_enabled, trace, Level};
use native_tls::{TlsConnector, TlsStream};
use std::{
    any::Any,
    cell::RefCell,
    collections::HashSet,
    convert::TryInto,
    io::{self, Read, Write},
    net::TcpStream,
    result, thread,
    time::Duration,
};
use thiserror::Error;
use utf7_imap::{decode_utf7_imap as decode_utf7, encode_utf7_imap as encode_utf7};

use crate::{
    account, backend, email, envelope, process, Backend, Email, Envelopes, Flags, Folder, Folders,
    ImapConfig,
};

#[cfg(feature = "imap-backend")]
use crate::flag::imap::ImapFlag;

#[derive(Error, Debug)]
pub enum Error {
    #[error("cannot get imap session: session not initialized")]
    GetSessionNotInitializedError,
    #[error("cannot get imap fetches: fetches not initialized")]
    GetFetchesNotInitializedError,

    #[error("cannot get imap backend from config")]
    GetBackendFromConfigError,
    #[error("cannot get envelope of message {0}")]
    GetEnvelopeError(u32),
    #[error("cannot get sender of message {0}")]
    GetSenderError(u32),
    #[error("cannot get imap session")]
    GetSessionError,
    #[error("cannot retrieve message {0}'s uid")]
    GetMsgUidError(u32),
    #[error("cannot find message {0}")]
    FindMsgError(String),
    #[error("cannot parse sort criterion {0}")]
    ParseSortCriterionError(String),

    #[error("cannot decode subject of message {1}")]
    DecodeSubjectError(#[source] rfc2047_decoder::Error, u32),
    #[error("cannot decode sender name of message {1}")]
    DecodeSenderNameError(#[source] rfc2047_decoder::Error, u32),
    #[error("cannot decode sender mailbox of message {1}")]
    DecodeSenderMboxError(#[source] rfc2047_decoder::Error, u32),
    #[error("cannot decode sender host of message {1}")]
    DecodeSenderHostError(#[source] rfc2047_decoder::Error, u32),

    #[error("cannot copy email(s) {1} from {2} to {3}")]
    CopyEmailError(#[source] imap::Error, String, String, String),
    #[error("cannot move email(s) {1} from {2} to {3}")]
    MoveEmailError(#[source] imap::Error, String, String, String),
    #[error("cannot create tls connector")]
    CreateTlsConnectorError(#[source] native_tls::Error),
    #[error("cannot connect to imap server")]
    ConnectImapServerError(#[source] imap::Error),
    #[error("cannot login to imap server")]
    LoginImapServerError(#[source] imap::Error),
    #[error("cannot search new messages")]
    SearchNewMsgsError(#[source] imap::Error),
    #[error("cannot examine mailbox {1}")]
    ExamineMboxError(#[source] imap::Error, String),
    #[error("cannot start the idle mode")]
    StartIdleModeError(#[source] imap::Error),
    #[error("cannot parse message {1}")]
    ParseMsgError(#[source] mailparse::MailParseError, String),
    #[error("cannot fetch new messages envelope")]
    FetchNewMsgsEnvelopeError(#[source] imap::Error),
    #[error("cannot get uid of message {0}")]
    GetUidError(u32),
    #[error("cannot create mailbox {1}")]
    CreateMboxError(#[source] imap::Error, String),
    #[error("cannot list mailboxes")]
    ListMboxesError(#[source] imap::Error),
    #[error("cannot delete mailbox {1}")]
    DeleteMboxError(#[source] imap::Error, String),
    #[error("cannot select mailbox {1}")]
    SelectFolderError(#[source] imap::Error, String),
    #[error("cannot fetch messages within range {1}")]
    FetchMsgsByRangeError(#[source] imap::Error, String),
    #[error("cannot fetch messages by sequence {1}")]
    FetchMsgsBySeqError(#[source] imap::Error, String),
    #[error("cannot append message to mailbox {1}")]
    AppendMsgError(#[source] imap::Error, String),
    #[error("cannot sort messages in mailbox {1} with query: {2}")]
    SortMsgsError(#[source] imap::Error, String, String),
    #[error("cannot search messages in mailbox {1} with query: {2}")]
    SearchMsgsError(#[source] imap::Error, String, String),
    #[error("cannot expunge mailbox {1}")]
    ExpungeError(#[source] imap::Error, String),
    #[error("cannot add flags {1} to message(s) {2}")]
    AddFlagsError(#[source] imap::Error, Flags, String),
    #[error("cannot set flags {1} to message(s) {2}")]
    SetFlagsError(#[source] imap::Error, Flags, String),
    #[error("cannot delete flags {1} to message(s) {2}")]
    DelFlagsError(#[source] imap::Error, Flags, String),
    #[error("cannot logout from imap server")]
    LogoutError(#[source] imap::Error),

    #[error(transparent)]
    ConfigError(#[from] account::config::Error),
    #[error(transparent)]
    ImapConfigError(#[from] backend::imap::config::Error),
    #[error(transparent)]
    MsgError(#[from] email::Error),
}

pub type Result<T> = result::Result<T, Error>;

enum ImapSessionStream {
    Tls(TlsStream<TcpStream>),
    Tcp(TcpStream),
}

impl SetReadTimeout for ImapSessionStream {
    fn set_read_timeout(&mut self, timeout: Option<std::time::Duration>) -> imap::Result<()> {
        match self {
            Self::Tls(stream) => stream.set_read_timeout(timeout),
            Self::Tcp(stream) => stream.set_read_timeout(timeout),
        }
    }
}

impl Read for ImapSessionStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Tls(stream) => stream.read(buf),
            Self::Tcp(stream) => stream.read(buf),
        }
    }
}

impl Write for ImapSessionStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tls(stream) => stream.write(buf),
            Self::Tcp(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tls(stream) => stream.flush(),
            Self::Tcp(stream) => stream.flush(),
        }
    }
}

type ImapSession = imap::Session<ImapSessionStream>;

pub struct ImapBackend<'a> {
    imap_config: &'a ImapConfig,
    session: RefCell<ImapSession>,
}

impl<'a> ImapBackend<'a> {
    pub fn new(imap_config: &'a ImapConfig) -> Result<Self> {
        let builder = TlsConnector::builder()
            .danger_accept_invalid_certs(imap_config.insecure())
            .danger_accept_invalid_hostnames(imap_config.insecure())
            .build()
            .map_err(Error::CreateTlsConnectorError)?;

        let mut client_builder = imap::ClientBuilder::new(&imap_config.host, imap_config.port);
        if imap_config.starttls() {
            client_builder.starttls();
        }

        let client = if imap_config.ssl() {
            client_builder.connect(|domain, tcp| {
                let connector = TlsConnector::connect(&builder, domain, tcp)?;
                Ok(ImapSessionStream::Tls(connector))
            })
        } else {
            client_builder.connect(|_, tcp| Ok(ImapSessionStream::Tcp(tcp)))
        }
        .map_err(Error::ConnectImapServerError)?;

        let mut session = client
            .login(&imap_config.login, &imap_config.passwd()?)
            .map_err(|res| Error::LoginImapServerError(res.0))?;
        session.debug = log_enabled!(Level::Trace);

        Ok(Self {
            imap_config,
            session: RefCell::new(session),
        })
    }

    fn search_new_msgs(&'a self, query: &str) -> Result<Vec<u32>> {
        let mut session = self.session.borrow_mut();

        let uids: Vec<u32> = session
            .uid_search(query)
            .map_err(Error::SearchNewMsgsError)?
            .into_iter()
            .collect();
        debug!("found {} new messages", uids.len());
        trace!("uids: {:?}", uids);

        Ok(uids)
    }

    pub fn notify(&'a self, keepalive: u64, mbox: &str) -> Result<()> {
        let mut session = self.session.borrow_mut();

        session
            .examine(mbox)
            .map_err(|err| Error::ExamineMboxError(err, mbox.to_owned()))?;

        debug!("init messages hashset");
        let mut msgs_set: HashSet<u32> = self
            .search_new_msgs(&self.imap_config.notify_query())?
            .iter()
            .cloned()
            .collect::<HashSet<_>>();
        trace!("messages hashset: {:?}", msgs_set);

        loop {
            debug!("begin loop");
            session
                .idle()
                .and_then(|mut idle| {
                    idle.set_keepalive(Duration::new(keepalive, 0));
                    idle.wait_keepalive_while(|res| {
                        // TODO: handle response
                        trace!("idle response: {:?}", res);
                        false
                    })
                })
                .map_err(Error::StartIdleModeError)?;

            let uids: Vec<u32> = self
                .search_new_msgs(&self.imap_config.notify_query())?
                .into_iter()
                .filter(|uid| -> bool { msgs_set.get(uid).is_none() })
                .collect();
            debug!("found {} new messages not in hashset", uids.len());
            trace!("messages hashet: {:?}", msgs_set);

            if !uids.is_empty() {
                let uids = uids
                    .iter()
                    .map(|uid| uid.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                let fetches = session
                    .uid_fetch(uids, "(UID ENVELOPE)")
                    .map_err(Error::FetchNewMsgsEnvelopeError)?;

                for fetch in fetches.iter() {
                    let msg = envelope::imap::from_raw(fetch)?;
                    let uid = fetch.uid.ok_or_else(|| Error::GetUidError(fetch.message))?;

                    let from = msg.sender.to_owned().into();
                    self.imap_config.run_notify_cmd(uid, &msg.subject, &from)?;

                    debug!("notify message: {}", uid);
                    trace!("message: {:?}", msg);

                    debug!("insert message {} in hashset", uid);
                    msgs_set.insert(uid);
                    trace!("messages hashset: {:?}", msgs_set);
                }
            }

            debug!("end loop");
        }
    }

    pub fn watch(&'a self, keepalive: u64, mbox: &str) -> Result<()> {
        debug!("examine folder: {}", mbox);
        let mut session = self.session.borrow_mut();

        session
            .examine(mbox)
            .map_err(|err| Error::ExamineMboxError(err, mbox.to_owned()))?;

        loop {
            debug!("begin loop");

            let cmds = self.imap_config.watch_cmds().clone();
            thread::spawn(move || {
                debug!("batch execution of {} cmd(s)", cmds.len());
                cmds.iter().for_each(|cmd| match process::run(cmd, &[]) {
                    // TODO: manage errors
                    Err(_) => (),
                    Ok(_) => (),
                })
            });

            session
                .idle()
                .and_then(|mut idle| {
                    idle.set_keepalive(Duration::new(keepalive, 0));
                    idle.wait_keepalive_while(|res| {
                        // TODO: handle response
                        trace!("idle response: {:?}", res);
                        false
                    })
                })
                .map_err(Error::StartIdleModeError)?;

            debug!("end loop");
        }
    }

    pub fn disconnect(&'a self) -> Result<()> {
        let mut session = self.session.borrow_mut();
        Ok(session.logout().map_err(Error::LogoutError)?)
    }
}

impl Backend for ImapBackend<'_> {
    fn add_folder(&self, folder: &str) -> backend::Result<()> {
        let mut session = self.session.borrow_mut();
        let folder = encode_utf7(folder.to_owned());

        session
            .create(&folder)
            .map_err(|err| Error::CreateMboxError(err, folder.to_owned()))?;

        Ok(())
    }

    fn list_folder(&self) -> backend::Result<Folders> {
        let mut session = self.session.borrow_mut();
        let imap_mboxes = session
            .list(Some(""), Some("*"))
            .map_err(Error::ListMboxesError)?;
        let mboxes = Folders(
            imap_mboxes
                .iter()
                .map(|imap_mbox| Folder {
                    delim: imap_mbox.delimiter().unwrap_or_default().into(),
                    name: decode_utf7(imap_mbox.name().into()),
                    desc: imap_mbox
                        .attributes()
                        .iter()
                        .map(|attr| match attr {
                            NameAttribute::Marked => "Marked",
                            NameAttribute::Unmarked => "Unmarked",
                            NameAttribute::NoSelect => "NoSelect",
                            NameAttribute::NoInferiors => "NoInferiors",
                            NameAttribute::Custom(custom) => custom.trim_start_matches('\\'),
                        })
                        .collect::<Vec<_>>()
                        .join(", "),
                })
                .collect(),
        );

        trace!("imap folders: {:?}", mboxes);
        Ok(mboxes)
    }

    fn delete_folder(&self, folder: &str) -> backend::Result<()> {
        let mut session = self.session.borrow_mut();
        let folder = encode_utf7(folder.to_owned());

        session
            .delete(&folder)
            .map_err(|err| Error::DeleteMboxError(err, folder.to_owned()))?;

        Ok(())
    }

    fn list_envelope(
        &self,
        folder: &str,
        page_size: usize,
        page: usize,
    ) -> backend::Result<Envelopes> {
        let mut session = self.session.borrow_mut();
        let folder = encode_utf7(folder.to_owned());
        let last_seq = session
            .select(&folder)
            .map_err(|err| Error::SelectFolderError(err, folder.to_owned()))?
            .exists as usize;
        debug!("last sequence number: {:?}", last_seq);
        if last_seq == 0 {
            return Ok(Envelopes::default());
        }

        let range = if page_size > 0 {
            let cursor = page * page_size;
            let begin = 1.max(last_seq - cursor.min(last_seq));
            let end = begin - begin.min(page_size) + 1;
            format!("{}:{}", end, begin)
        } else {
            String::from("1:*")
        };
        debug!("range: {:?}", range);

        let fetches = session
            .fetch(&range, "(ENVELOPE FLAGS INTERNALDATE)")
            .map_err(|err| Error::FetchMsgsByRangeError(err, range.to_owned()))?;

        let envelopes = envelope::imap::from_raws(fetches)?;
        Ok(envelopes)
    }

    fn search_envelope(
        &self,
        folder: &str,
        query: &str,
        sort: &str,
        page_size: usize,
        page: usize,
    ) -> backend::Result<Envelopes> {
        let mut session = self.session.borrow_mut();
        let folder = encode_utf7(folder.to_owned());
        let last_seq = session
            .select(&folder)
            .map_err(|err| Error::SelectFolderError(err, folder.to_owned()))?
            .exists;
        debug!("last sequence number: {:?}", last_seq);
        if last_seq == 0 {
            return Ok(Envelopes::default());
        }

        let begin = page * page_size;
        let end = begin + (page_size - 1);
        let seqs: Vec<String> = if sort.is_empty() {
            session
                .search(query)
                .map_err(|err| Error::SearchMsgsError(err, folder.to_owned(), query.to_owned()))?
                .iter()
                .map(|seq| seq.to_string())
                .collect()
        } else {
            let sort: envelope::imap::SortCriteria = sort.try_into()?;
            session
                .sort(&sort, imap::extensions::sort::SortCharset::Utf8, query)
                .map_err(|err| Error::SortMsgsError(err, folder.to_owned(), query.to_owned()))?
                .iter()
                .map(|seq| seq.to_string())
                .collect()
        };
        if seqs.is_empty() {
            return Ok(Envelopes::default());
        }

        let range = seqs[begin..end.min(seqs.len())].join(",");
        let fetches = session
            .fetch(&range, "(ENVELOPE FLAGS INTERNALDATE)")
            .map_err(|err| Error::FetchMsgsByRangeError(err, range.to_owned()))?;

        let envelopes = envelope::imap::from_raws(fetches)?;
        Ok(envelopes)
    }

    fn add_email(&self, folder: &str, email: &[u8], flags: &str) -> backend::Result<String> {
        let mut session = self.session.borrow_mut();
        let folder = encode_utf7(folder.to_owned());
        let flags = Flags::from(flags);
        session
            .append(&folder, email)
            .flags(<Flags as Into<Vec<ImapFlag>>>::into(flags))
            .finish()
            .map_err(|err| Error::AppendMsgError(err, folder.to_owned()))?;
        let last_seq = session
            .select(&folder)
            .map_err(|err| Error::SelectFolderError(err, folder.to_owned()))?
            .exists;
        Ok(last_seq.to_string())
    }

    fn get_email(&self, folder: &str, seq: &str) -> backend::Result<Email> {
        debug!("folder: {:?}", folder);
        debug!("seq: {:?}", seq);

        let folder = encode_utf7(folder.to_owned());
        debug!("utf7 encoded folder: {:?}", folder);

        let mut session = self.session.borrow_mut();

        session
            .select(&folder)
            .map_err(|err| Error::SelectFolderError(err, folder.to_owned()))?;

        let fetches = session
            .fetch(seq, "BODY[]")
            .map_err(|err| Error::FetchMsgsBySeqError(err, seq.to_owned()))?;
        let email = Email::try_from(fetches)?;
        trace!("email: {:?}", email);

        Ok(email)
    }

    fn copy_email(&self, folder: &str, folder_target: &str, ids: &str) -> backend::Result<()> {
        debug!("ids: {}", ids);
        debug!("source folder: {}", folder);
        debug!("target folder: {}", folder_target);

        let encoded_folder = encode_utf7(folder.to_owned());
        let encoded_folder_target = encode_utf7(folder_target.to_owned());
        debug!("source folder (utf7 encoded): {}", encoded_folder);
        debug!("target folder (utf7 encoded): {}", encoded_folder_target);

        let mut session = self.session.borrow_mut();

        session
            .select(encoded_folder)
            .map_err(|err| Error::SelectFolderError(err, folder.to_owned()))?;

        session.copy(ids, encoded_folder_target).map_err(|err| {
            Error::CopyEmailError(
                err,
                ids.to_owned(),
                folder.to_owned(),
                folder_target.to_owned(),
            )
        })?;

        Ok(())
    }

    fn move_email(&self, folder: &str, folder_target: &str, ids: &str) -> backend::Result<()> {
        debug!("ids: {}", ids);
        debug!("source folder: {}", folder);
        debug!("target folder: {}", folder_target);

        let encoded_folder = encode_utf7(folder.to_owned());
        let encoded_folder_target = encode_utf7(folder_target.to_owned());
        debug!("source folder (utf7 encoded): {}", encoded_folder);
        debug!("target folder (utf7 encoded): {}", encoded_folder_target);

        let mut session = self.session.borrow_mut();

        session
            .select(encoded_folder)
            .map_err(|err| Error::SelectFolderError(err, folder.to_owned()))?;

        session.mv(ids, encoded_folder_target).map_err(|err| {
            Error::MoveEmailError(
                err,
                ids.to_owned(),
                folder.to_owned(),
                folder_target.to_owned(),
            )
        })?;

        Ok(())
    }

    fn delete_email(&self, folder: &str, seq: &str) -> backend::Result<()> {
        self.add_flags(folder, seq, "deleted")
    }

    fn add_flags(&self, folder: &str, seq_range: &str, flags: &str) -> backend::Result<()> {
        let mut session = self.session.borrow_mut();
        let folder = encode_utf7(folder.to_owned());
        let flags: Flags = flags.into();
        session
            .select(&folder)
            .map_err(|err| Error::SelectFolderError(err, folder.to_owned()))?;
        session
            .store(seq_range, format!("+FLAGS ({})", flags))
            .map_err(|err| Error::AddFlagsError(err, flags.to_owned(), seq_range.to_owned()))?;
        session
            .expunge()
            .map_err(|err| Error::ExpungeError(err, folder.to_owned()))?;
        Ok(())
    }

    fn set_flags(&self, folder: &str, seq_range: &str, flags: &str) -> backend::Result<()> {
        let mut session = self.session.borrow_mut();
        let folder = encode_utf7(folder.to_owned());
        let flags: Flags = flags.into();
        session
            .select(&folder)
            .map_err(|err| Error::SelectFolderError(err, folder.to_owned()))?;
        session
            .store(seq_range, format!("FLAGS ({})", flags))
            .map_err(|err| Error::SetFlagsError(err, flags.to_owned(), seq_range.to_owned()))?;
        Ok(())
    }

    fn remove_flags(&self, folder: &str, seq_range: &str, flags: &str) -> backend::Result<()> {
        let mut session = self.session.borrow_mut();
        let folder = encode_utf7(folder.to_owned());
        let flags: Flags = flags.into();
        session
            .select(&folder)
            .map_err(|err| Error::SelectFolderError(err, folder.to_owned()))?;
        session
            .store(seq_range, format!("-FLAGS ({})", flags))
            .map_err(|err| Error::DelFlagsError(err, flags.to_owned(), seq_range.to_owned()))?;
        Ok(())
    }

    fn as_any(&'static self) -> &(dyn Any) {
        self
    }
}
