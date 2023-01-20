use env_logger;
use std::{
    borrow::Cow,
    collections::HashSet,
    env::temp_dir,
    fs::{create_dir_all, remove_dir_all},
    thread,
    time::Duration,
};

use himalaya_lib::{
    envelope, folder, AccountConfig, Backend, CompilerBuilder, Flag, Flags, ImapBackendBuilder,
    ImapConfig, MaildirBackend, MaildirConfig, ThreadSafeBackend, TplBuilder, DEFAULT_INBOX_FOLDER,
    DEFAULT_SENT_FOLDER,
};

#[test]
fn test_sync() {
    env_logger::builder().is_test(true).init();

    // set up account

    let sync_dir = temp_dir().join("himalaya-sync");
    if sync_dir.is_dir() {
        remove_dir_all(&sync_dir).unwrap();
    }
    create_dir_all(&sync_dir).unwrap();

    let account = AccountConfig {
        name: "account".into(),
        sync: true,
        sync_dir: Some(sync_dir.clone()),
        ..AccountConfig::default()
    };

    // set up imap backend

    let imap = ImapBackendBuilder::default()
        .pool_size(10)
        .build(
            Cow::Borrowed(&account),
            Cow::Owned(ImapConfig {
                host: "localhost".into(),
                port: 3143,
                ssl: Some(false),
                starttls: Some(false),
                insecure: Some(true),
                login: "bob@localhost".into(),
                passwd_cmd: "echo 'password'".into(),
                ..ImapConfig::default()
            }),
        )
        .unwrap();

    // reset folders

    for folder in imap.list_folders().unwrap().iter() {
        match folder.name.as_str() {
            DEFAULT_INBOX_FOLDER | DEFAULT_SENT_FOLDER => imap.purge_folder(&folder.name).unwrap(),
            folder => imap.delete_folder(folder).unwrap(),
        }
    }

    // add three emails to folder INBOX with delay (in order to have a
    // different date)

    imap.add_email(
        "INBOX",
        &TplBuilder::default()
            .message_id("<a@localhost>")
            .from("alice@localhost")
            .to("bob@localhost")
            .subject("A")
            .text_plain_part("A")
            .compile(CompilerBuilder::default())
            .unwrap(),
        &Flags::default(),
    )
    .unwrap();

    thread::sleep(Duration::from_secs(1));

    imap.add_email(
        "INBOX",
        &TplBuilder::default()
            .message_id("<b@localhost>")
            .from("alice@localhost")
            .to("bob@localhost")
            .subject("B")
            .text_plain_part("B")
            .compile(CompilerBuilder::default())
            .unwrap(),
        &Flags::from_iter([Flag::Flagged]),
    )
    .unwrap();

    thread::sleep(Duration::from_secs(1));

    imap.add_email(
        "INBOX",
        &TplBuilder::default()
            .message_id("<c@localhost>")
            .from("alice@localhost")
            .to("bob@localhost")
            .subject("C")
            .text_plain_part("C")
            .compile(CompilerBuilder::default())
            .unwrap(),
        &Flags::default(),
    )
    .unwrap();

    let imap_inbox_envelopes = imap.list_envelopes("INBOX", 0, 0).unwrap();

    // add two more emails to folder Sent

    imap.add_email(
        "Sent",
        &TplBuilder::default()
            .message_id("<d@localhost>")
            .from("alice@localhost")
            .to("bob@localhost")
            .subject("D")
            .text_plain_part("D")
            .compile(CompilerBuilder::default())
            .unwrap(),
        &Flags::default(),
    )
    .unwrap();

    thread::sleep(Duration::from_secs(1));

    imap.add_email(
        "Sent",
        &TplBuilder::default()
            .message_id("<e@localhost>")
            .from("alice@localhost")
            .to("bob@localhost")
            .subject("E")
            .text_plain_part("E")
            .compile(CompilerBuilder::default())
            .unwrap(),
        &Flags::default(),
    )
    .unwrap();

    // init maildir backend reader

    let mdir = MaildirBackend::new(
        Cow::Borrowed(&account),
        Cow::Owned(MaildirConfig {
            root_dir: sync_dir.join(&account.name),
        }),
    )
    .unwrap();

    let imap_sent_envelopes = imap.list_envelopes("Sent", 0, 0).unwrap();

    // sync imap account twice in a row to see if all work as expected
    // without duplicate items

    imap.sync(&account).unwrap();
    imap.sync(&account).unwrap();

    // check folders integrity

    let imap_folders = imap.list_folders().unwrap();
    assert_eq!(imap_folders, mdir.list_folders().unwrap());
    assert_eq!(
        imap_folders
            .iter()
            .map(|f| f.name.clone())
            .collect::<Vec<_>>(),
        vec!["INBOX", "Sent"]
    );

    // check maildir envelopes integrity

    let mdir_inbox_envelopes = mdir.list_envelopes("INBOX", 0, 0).unwrap();
    assert_eq!(imap_inbox_envelopes, mdir_inbox_envelopes);

    let mdir_sent_envelopes = mdir.list_envelopes("Sent", 0, 0).unwrap();
    assert_eq!(imap_sent_envelopes, mdir_sent_envelopes);

    // check maildir emails content integrity

    let ids = mdir_inbox_envelopes.iter().map(|e| e.id.as_str()).collect();
    let emails = mdir.get_emails("INBOX", ids).unwrap();
    let emails = emails.to_vec();
    assert_eq!(3, emails.len());
    assert_eq!("C\r\n", emails[0].parsed().unwrap().get_body().unwrap());
    assert_eq!("B\r\n", emails[1].parsed().unwrap().get_body().unwrap());
    assert_eq!("A\r\n", emails[2].parsed().unwrap().get_body().unwrap());

    let ids = mdir_sent_envelopes.iter().map(|e| e.id.as_str()).collect();
    let emails = mdir.get_emails("Sent", ids).unwrap();
    let emails = emails.to_vec();
    assert_eq!(2, emails.len());
    assert_eq!("E\r\n", emails[0].parsed().unwrap().get_body().unwrap());
    assert_eq!("D\r\n", emails[1].parsed().unwrap().get_body().unwrap());

    // check folders cache integrity

    let cache = folder::sync::Cache::new(Cow::Borrowed(&account), &sync_dir).unwrap();

    assert_eq!(
        HashSet::from_iter(["INBOX".into(), "Sent".into()]),
        cache.list_local_folders().unwrap()
    );

    assert_eq!(
        HashSet::from_iter(["INBOX".into(), "Sent".into()]),
        cache.list_remote_folders().unwrap()
    );

    // check envelopes cache integrity

    let cache = envelope::sync::Cache::new(Cow::Borrowed(&account), &sync_dir).unwrap();

    let mdir_inbox_envelopes_cached = cache.list_local_envelopes("INBOX").unwrap();
    let imap_inbox_envelopes_cached = cache.list_remote_envelopes("INBOX").unwrap();

    assert_eq!(mdir_inbox_envelopes, mdir_inbox_envelopes_cached);
    assert_eq!(imap_inbox_envelopes, imap_inbox_envelopes_cached);

    let mdir_sent_envelopes_cached = cache.list_local_envelopes("Sent").unwrap();
    let imap_sent_envelopes_cached = cache.list_remote_envelopes("Sent").unwrap();

    assert_eq!(mdir_sent_envelopes, mdir_sent_envelopes_cached);
    assert_eq!(imap_sent_envelopes, imap_sent_envelopes_cached);

    // remove emails and update flags from both side, sync again and
    // check integrity

    imap.delete_emails_internal("INBOX", vec![&imap_inbox_envelopes[0].internal_id])
        .unwrap();
    imap.add_flags_internal(
        "INBOX",
        vec![&imap_inbox_envelopes[1].internal_id],
        &Flags::from_iter([Flag::Draft]),
    )
    .unwrap();
    mdir.delete_emails_internal("INBOX", vec![&mdir_inbox_envelopes[2].internal_id])
        .unwrap();
    mdir.add_flags_internal(
        "INBOX",
        vec![&mdir_inbox_envelopes[1].internal_id],
        &Flags::from_iter([Flag::Flagged, Flag::Answered]),
    )
    .unwrap();

    imap.sync(&account).unwrap();

    let imap_envelopes = imap.list_envelopes("INBOX", 0, 0).unwrap();
    let mdir_envelopes = mdir.list_envelopes("INBOX", 0, 0).unwrap();
    assert_eq!(imap_envelopes, mdir_envelopes);

    let cached_mdir_envelopes = cache.list_local_envelopes("INBOX").unwrap();
    assert_eq!(cached_mdir_envelopes, mdir_envelopes);

    let cached_imap_envelopes = cache.list_remote_envelopes("INBOX").unwrap();
    assert_eq!(cached_imap_envelopes, imap_envelopes);
}
