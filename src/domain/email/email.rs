use imap::types::{Fetch, ZeroCopy};
use lettre::{address::AddressError, message::Mailboxes};
use log::{trace, warn};
use mailparse::{DispositionType, MailHeaderMap, MailParseError, ParsedMail};
use std::{fmt::Debug, io, path::PathBuf};
use thiserror::Error;
use tree_magic;

use crate::{
    account, sanitize_text_plain_part, AccountConfig, Attachment, Parts, PartsIterator, Tpl,
    TplBuilder, TplBuilderOpts, DEFAULT_SIGNATURE_DELIM,
};

use super::tpl::ShowHeaders;

#[derive(Error, Debug)]
pub enum EmailError {
    #[error("cannot parse email")]
    ParseEmailError(#[source] MailParseError),
    #[error("cannot parse email body")]
    ParseEmailBodyError(#[source] MailParseError),
    #[error("cannot parse email: raw email is empty")]
    ParseEmailEmptyRawError,
    #[error("cannot parse message or address")]
    ParseEmailAddressError(#[from] AddressError),
    #[error("cannot delete local draft at {1}")]
    DeleteLocalDraftError(#[source] io::Error, PathBuf),

    #[cfg(feature = "imap-backend")]
    #[error("cannot parse email from imap fetches: empty fetches")]
    ParseEmailFromImapFetchesEmptyError,

    #[error(transparent)]
    ConfigError(#[from] account::config::Error),

    // TODO: sort me
    #[error("cannot get content type of multipart")]
    GetMultipartContentTypeError,
    #[error("cannot find encrypted part of multipart")]
    GetEncryptedPartMultipartError,
    #[error("cannot parse encrypted part of multipart")]
    ParseEncryptedPartError(#[source] mailparse::MailParseError),
    #[error("cannot get body from encrypted part")]
    GetEncryptedPartBodyError(#[source] mailparse::MailParseError),
    #[error("cannot write encrypted part to temporary file")]
    WriteEncryptedPartBodyError(#[source] io::Error),
    #[error("cannot write encrypted part to temporary file")]
    DecryptPartError(#[source] account::config::Error),
}

#[derive(Debug)]
pub enum RawEmail<'a> {
    Vec(Vec<u8>),
    Bytes(&'a [u8]),
    #[cfg(feature = "imap-backend")]
    ImapFetches(ZeroCopy<Vec<Fetch>>),
}

#[derive(Debug)]
pub struct Email<'a> {
    raw: RawEmail<'a>,
    parsed: Option<ParsedMail<'a>>,
}

impl<'a> Email<'a> {
    pub fn parsed(&'a mut self) -> Result<&ParsedMail<'a>, EmailError> {
        if self.parsed.is_none() {
            self.parsed = Some(match &self.raw {
                RawEmail::Vec(vec) => {
                    mailparse::parse_mail(vec).map_err(EmailError::ParseEmailError)
                }
                RawEmail::Bytes(bytes) => {
                    mailparse::parse_mail(*bytes).map_err(EmailError::ParseEmailError)
                }
                #[cfg(feature = "imap-backend")]
                RawEmail::ImapFetches(fetches) => {
                    let body = fetches
                        .first()
                        .and_then(|fetch| fetch.body())
                        .ok_or(EmailError::ParseEmailFromImapFetchesEmptyError)?;
                    mailparse::parse_mail(body).map_err(EmailError::ParseEmailError)
                }
            }?)
        }

        self.parsed
            .as_ref()
            .ok_or_else(|| EmailError::ParseEmailEmptyRawError)
    }

    pub fn attachments(&'a mut self) -> Result<Vec<Attachment>, EmailError> {
        let attachments = PartsIterator::new(self.parsed()?).filter_map(|part| {
            let cdisp = part.get_content_disposition();
            if let DispositionType::Attachment = cdisp.disposition {
                let filename = cdisp.params.get("filename");
                let body = part
                    .get_body_raw()
                    .map_err(|err| {
                        let filename = filename
                            .map(|f| format!("attachment {}", f))
                            .unwrap_or_else(|| "unknown attachment".into());
                        warn!("skipping {}: {}", filename, err);
                        trace!("skipping part: {:#?}", part);
                        err
                    })
                    .ok()?;

                Some(Attachment {
                    filename: filename.map(String::from),
                    mime: tree_magic::from_u8(&body),
                    body,
                })
            } else {
                None
            }
        });

        Ok(attachments.collect())
    }

    pub fn text_parts(
        &'a self,
        parsed: &'a ParsedMail,
    ) -> Result<Vec<&ParsedMail<'a>>, EmailError> {
        let text_parts = PartsIterator::new(parsed).filter_map(|part| {
            if part.ctype.mimetype.starts_with("text") {
                Some(part)
            } else {
                None
            }
        });

        Ok(text_parts.collect())
    }

    pub fn as_raw(&self) -> Result<&[u8], EmailError> {
        match self.raw {
            RawEmail::Vec(ref vec) => Ok(vec),
            RawEmail::Bytes(bytes) => Ok(bytes),
            #[cfg(feature = "imap-backend")]
            RawEmail::ImapFetches(ref fetches) => fetches
                .first()
                .and_then(|fetch| fetch.body())
                .ok_or_else(|| EmailError::ParseEmailFromImapFetchesEmptyError),
        }
    }

    pub fn to_read_tpl(
        &'a mut self,
        config: &'a AccountConfig,
        opts: TplBuilderOpts,
    ) -> Result<Tpl, EmailError> {
        let mut tpl = TplBuilder::default();

        let parsed = self.parsed()?;
        let parsed_headers = parsed.get_headers();

        if let Some(show_headers) = opts.show_headers {
            match show_headers {
                ShowHeaders::All => {
                    for header in parsed_headers {
                        tpl = tpl.header(header.get_key(), header.get_value())
                    }
                }
                ShowHeaders::Only(ref headers) => {
                    for header in headers {
                        if let Some(header) = parsed_headers.get_first_header(header) {
                            tpl = tpl.header(header.get_key(), header.get_value())
                        }
                    }
                }
            }
        } else {
            for header in &config.email_reading_headers() {
                if let Some(header) = parsed_headers.get_first_header(header) {
                    tpl = tpl.header(header.get_key(), header.get_value())
                }
            }
        };

        let opts = TplBuilderOpts {
            show_headers: Some(ShowHeaders::Only(tpl.headers_order.clone())),
            ..opts
        };

        for part in PartsIterator::new(parsed) {
            match part.ctype.mimetype.as_str() {
                "text/plain" => {
                    tpl =
                        tpl.text_plain_part(part.get_body().map_err(EmailError::ParseEmailError)?);
                }
                // TODO: manage other mime types
                _ => (),
            }
        }

        Ok(tpl.build(opts))
    }

    pub fn to_reply_tpl(
        &'a mut self,
        config: &AccountConfig,
        all: bool,
    ) -> Result<Tpl, EmailError> {
        let mut tpl = TplBuilder::default();

        let parsed = self.parsed()?;
        let parsed_headers = parsed.get_headers();
        let sender = config.addr()?;

        // From

        tpl = tpl.from(&sender);

        // To

        tpl = tpl.to({
            let mut all_mboxes = Mailboxes::new();

            let from = parsed_headers.get_all_values("From");
            let to = parsed_headers.get_all_values("To");
            let reply_to = parsed_headers.get_all_values("Reply-To");

            let reply_to_iter = if reply_to.is_empty() {
                from.into_iter()
            } else {
                reply_to.into_iter()
            };

            for reply_to in reply_to_iter {
                let mboxes: Mailboxes = reply_to.parse()?;
                all_mboxes.extend(mboxes.into_iter().filter(|mbox| mbox.email != sender.email));
            }

            for reply_to in to.into_iter() {
                let mboxes: Mailboxes = reply_to.parse()?;
                all_mboxes.extend(mboxes.into_iter().filter(|mbox| mbox.email != sender.email));
            }

            if all {
                all_mboxes
            } else {
                all_mboxes
                    .into_single()
                    .map(|mbox| Mailboxes::from_iter([mbox]))
                    .unwrap_or_default()
            }
        });

        // In-Reply-To

        if let Some(ref message_id) = parsed_headers.get_first_value("Message-Id") {
            tpl = tpl.in_reply_to(message_id);
        }

        // Cc

        if all {
            tpl = tpl.cc({
                let mut cc = Mailboxes::new();

                for mboxes in parsed_headers.get_all_values("Cc") {
                    let mboxes: Mailboxes = mboxes.parse()?;
                    cc.extend(mboxes.into_iter().filter(|mbox| mbox.email != sender.email))
                }

                cc
            });
        }

        // Subject

        if let Some(ref subject) = parsed_headers.get_first_value("Subject") {
            tpl = tpl.subject(String::from("Re: ") + subject);
        }

        // Body

        tpl = tpl.text_plain_part({
            let mut lines = String::default();

            for part in PartsIterator::new(&parsed) {
                if part.ctype.mimetype != "text/plain" {
                    continue;
                }

                let body = sanitize_text_plain_part(
                    part.get_body().map_err(EmailError::ParseEmailBodyError)?,
                );

                lines.push_str("\n\n");

                for line in body.lines() {
                    // removes existing signature from the original body
                    if line[..] == DEFAULT_SIGNATURE_DELIM[0..3] {
                        break;
                    }

                    lines.push('>');
                    if !line.starts_with('>') {
                        lines.push_str(" ")
                    }
                    lines.push_str(line);
                    lines.push_str("\n");
                }
            }

            if let Some(ref signature) = config.signature()? {
                lines.push_str("\n");
                lines.push_str(signature);
            }

            lines
        });

        Ok(tpl.build(TplBuilderOpts::default()))
    }

    pub fn to_forward_tpl(&'a mut self, config: &AccountConfig) -> Result<Tpl, EmailError> {
        let mut tpl = Tpl::default();
        let parsed = self.parsed()?;
        let headers = parsed.get_headers();
        let sender = config.addr()?;

        // From

        tpl.push_header("From", &sender.to_string());

        // To

        tpl.push_header("To", "");

        // Subject

        let subject = headers.get_first_value("Subject").unwrap_or_default();
        tpl.push_header("Subject", format!("Fwd: {}", subject));

        // Signature

        if let Some(ref sig) = config.signature()? {
            tpl.push_str("\n");
            tpl.push_str(sig);
            tpl.push_str("\n");
        }

        // Body

        tpl.push_str("\n-------- Forwarded Message --------\n");
        tpl.push_header("Subject", subject);
        if let Some(date) = headers.get_first_value("date") {
            tpl.push_header("Date: ", date);
        }
        tpl.push_header("From: ", headers.get_all_values("from").join(", "));
        tpl.push_header("To: ", headers.get_all_values("to").join(", "));
        tpl.push_str("\n");
        tpl.push_str(&Parts::concat_text_plain_bodies(&parsed)?);

        Ok(tpl)
    }
}

impl<'a> From<Vec<u8>> for Email<'a> {
    fn from(vec: Vec<u8>) -> Self {
        Self {
            raw: RawEmail::Vec(vec),
            parsed: None,
        }
    }
}

impl<'a> From<&'a [u8]> for Email<'a> {
    fn from(bytes: &'a [u8]) -> Self {
        Self {
            raw: RawEmail::Bytes(bytes),
            parsed: None,
        }
    }
}

impl<'a> From<&'a str> for Email<'a> {
    fn from(str: &'a str) -> Self {
        str.as_bytes().into()
    }
}

impl<'a> From<ParsedMail<'a>> for Email<'a> {
    fn from(parsed: ParsedMail<'a>) -> Self {
        Self {
            raw: RawEmail::Bytes(parsed.raw_bytes),
            parsed: Some(parsed),
        }
    }
}

#[cfg(feature = "imap-backend")]
impl TryFrom<ZeroCopy<Vec<Fetch>>> for Email<'_> {
    type Error = EmailError;

    fn try_from(fetches: ZeroCopy<Vec<Fetch>>) -> Result<Self, Self::Error> {
        if fetches.is_empty() {
            Err(EmailError::ParseEmailFromImapFetchesEmptyError)
        } else {
            Ok(Self {
                raw: RawEmail::ImapFetches(fetches),
                parsed: None,
            })
        }
    }
}

#[cfg(test)]
mod test_to_read_tpl {
    use concat_with::concat_line;

    use crate::{AccountConfig, Email, TplBuilderOpts};

    #[test]
    fn test_default() {
        let config = AccountConfig::default();
        let opts = TplBuilderOpts::default();

        let mut email = Email::from(concat_line!(
            "From: from@localhost",
            "To: to@localhost",
            "Subject: subject",
            "",
            "Hello!",
            "",
            "-- ",
            "Regards,"
        ));

        let tpl = email.to_read_tpl(&config, opts).unwrap();

        let expected_tpl = concat_line!("Hello!", "", "-- ", "Regards,");

        assert_eq!(expected_tpl, *tpl);
    }

    #[test]
    fn test_email_reading_headers() {
        let config = AccountConfig {
            email_reading_headers: Some(vec![
                // existing headers
                "From".into(),
                "Subject".into(),
                // nonexisting headers
                "Cc".into(),
                "Bcc".into(),
            ]),
            ..AccountConfig::default()
        };

        let opts = TplBuilderOpts::default();

        let mut email = Email::from(concat_line!(
            "From: from@localhost",
            "To: to@localhost",
            "Subject: subject",
            "",
            "Hello!",
            "",
            "-- ",
            "Regards,"
        ));

        let tpl = email.to_read_tpl(&config, opts).unwrap();

        let expected_tpl = concat_line!(
            "From: from@localhost",
            "Subject: subject",
            "",
            "Hello!",
            "",
            "-- ",
            "Regards,"
        );

        assert_eq!(expected_tpl, *tpl);
    }

    #[test]
    fn test_show_all_headers() {
        let config = AccountConfig {
            // config should be overriden by the options
            email_reading_headers: Some(vec!["Content-Type".into()]),
            ..AccountConfig::default()
        };

        let opts = TplBuilderOpts::default().show_all_headers();

        let mut email = Email::from(concat_line!(
            "From: from@localhost",
            "To: to@localhost",
            "Subject: subject",
            "",
            "Hello!",
            "",
            "-- ",
            "Regards,"
        ));

        let tpl = email.to_read_tpl(&config, opts).unwrap();

        let expected_tpl = concat_line!(
            "From: from@localhost",
            "To: to@localhost",
            "Subject: subject",
            "",
            "Hello!",
            "",
            "-- ",
            "Regards,"
        );

        assert_eq!(expected_tpl, *tpl);
    }

    #[test]
    fn test_show_only_headers() {
        let config = AccountConfig {
            // config should be overriden by the options
            email_reading_headers: Some(vec!["From".into()]),
            ..AccountConfig::default()
        };

        let opts = TplBuilderOpts::default().show_headers(
            [
                // existing headers
                "Subject",
                "To",
                // nonexisting header
                "Content-Type",
            ]
            .iter(),
        );

        let mut email = Email::from(concat_line!(
            "From: from@localhost",
            "To: to@localhost",
            "Subject: subject",
            "",
            "Hello!",
            "",
            "-- ",
            "Regards,"
        ));

        let tpl = email.to_read_tpl(&config, opts).unwrap();

        let expected_tpl = concat_line!(
            "Subject: subject",
            "To: to@localhost",
            "",
            "Hello!",
            "",
            "-- ",
            "Regards,"
        );

        assert_eq!(expected_tpl, *tpl);
    }
}

#[cfg(test)]
mod test_to_reply_tpl {
    use concat_with::concat_line;

    use crate::{AccountConfig, Email};

    #[test]
    fn test_default() {
        let config = AccountConfig {
            email: "to@localhost".into(),
            ..AccountConfig::default()
        };

        let mut email = Email::from(concat_line!(
            "From: from@localhost",
            "To: to@localhost, to2@localhost",
            "Cc: cc@localhost, cc2@localhost",
            "Bcc: bcc@localhost",
            "Subject: subject",
            "",
            "Hello!",
            "",
            "-- ",
            "Regards,"
        ));

        let tpl = email.to_reply_tpl(&config, false).unwrap();

        let expected_tpl = concat_line!(
            "From: to@localhost",
            "To: from@localhost",
            "Subject: Re: subject",
            "",
            "",
            "",
            "> Hello!",
            "> ",
            ""
        );

        assert_eq!(expected_tpl, *tpl);
    }

    #[test]
    fn test_reply_all() {
        let config = AccountConfig {
            email: "to@localhost".into(),
            ..AccountConfig::default()
        };

        let mut email = Email::from(concat_line!(
            "From: from@localhost",
            "To: to@localhost, to2@localhost",
            "Cc: to@localhost, cc@localhost, cc2@localhost",
            "Bcc: bcc@localhost",
            "Subject: subject",
            "",
            "Hello!",
            "",
            "-- ",
            "Regards,"
        ));

        let tpl = email.to_reply_tpl(&config, true).unwrap();

        let expected_tpl = concat_line!(
            "From: to@localhost",
            "To: from@localhost, to2@localhost",
            "Cc: cc@localhost, cc2@localhost",
            "Subject: Re: subject",
            "",
            "",
            "",
            "> Hello!",
            "> ",
            ""
        );

        assert_eq!(expected_tpl, *tpl);
    }

    #[test]
    fn test_signature() {
        let config = AccountConfig {
            email: "to@localhost".into(),
            signature: Some("Cordialement,".into()),
            ..AccountConfig::default()
        };

        let mut email = Email::from(concat_line!(
            "From: from@localhost",
            "To: to@localhost",
            "Subject: subject",
            "",
            "Hello!",
            "",
            "-- ",
            "Regards,"
        ));

        let tpl = email.to_reply_tpl(&config, false).unwrap();

        let expected_tpl = concat_line!(
            "From: to@localhost",
            "To: from@localhost",
            "Subject: Re: subject",
            "",
            "",
            "",
            "> Hello!",
            "> ",
            "",
            "-- ",
            "Cordialement,"
        );

        assert_eq!(expected_tpl, *tpl);
    }
}
