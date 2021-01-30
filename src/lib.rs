use async_stream::stream;
use futures_core::stream::Stream;
use log::{debug, error};
use pcsc::*;
use std::error::Error as StdError;
use std::thread;
use std::time::Duration;
use tokio::sync::mpsc;

fn is_dead(rs: &ReaderState) -> bool {
    rs.event_state().intersects(State::UNKNOWN | State::IGNORE)
}

#[derive(Debug)]
enum MeteCardState {
    UnsupportedApplicationSelect,
    ApplicationUnknown,
    InvalidAnswer,
    Uuid(String),
}

fn get_mete_card_state(card: &Card) -> Result<MeteCardState, Box<dyn StdError>> {
    // Send KalkGetränk APDU
    let apdu = b"\x00\xA4\x04\x00\x0d\xff\x4b\x61\x6c\x6b\x47\x65\x74\x72\xc3\xa4\x6e\x6b\x00";
    debug!("Sending APDU: {:?}", apdu);
    let mut rapdu_buf = [0; MAX_BUFFER_SIZE];
    let rapdu = card.transmit(apdu, &mut rapdu_buf)?;
    debug!("APDU response: {:x?}", rapdu);
    let l = rapdu.len();
    if l == 0 {
        return Ok(MeteCardState::UnsupportedApplicationSelect);
    }
    if l < 2 || rapdu[l - 2] != 0x90 || rapdu[l - 1] != 0x00 {
        return Ok(MeteCardState::ApplicationUnknown);
    }

    let apdu = b"\xd0\x00\x00\x00\x24";
    debug!("Sending APDU: {:?}", apdu);
    let rapdu = card.transmit(apdu, &mut rapdu_buf)?;
    debug!("APDU response: {:x?}", rapdu);
    let l = rapdu.len();
    if l == 0 {
        return Ok(MeteCardState::UnsupportedApplicationSelect);
    }
    if l != 38 || rapdu[l - 2] != 0x90 || rapdu[l - 1] != 0x00 {
        return Ok(MeteCardState::InvalidAnswer);
    }
    let s = std::str::from_utf8(&rapdu[0..l - 2])?.to_owned();

    Ok(MeteCardState::Uuid(s))
}

fn get_uid(card: &Card) -> Result<Option<Vec<u8>>, Box<dyn StdError>> {
    let apdu = &[0xFF, 0xCA, 0x00, 0x00, 0x00];
    debug!("Sending APDU: {:?}", apdu);
    let mut rapdu_buf = [0; MAX_BUFFER_SIZE];
    let rapdu = card.transmit(apdu, &mut rapdu_buf)?;
    debug!("APDU response: {:x?}", rapdu);
    let l = rapdu.len();
    // min length 3: at least one u8 as id + successful response
    if l < 3 || rapdu[l - 2] != 0x90 || rapdu[l - 1] != 0x00 {
        Ok(None)
    } else {
        Ok(Some(rapdu[0..l - 2].to_vec()))
    }
}

fn parse_card(card: Card) -> Result<Option<CardDetail>, Box<dyn StdError>> {
    let result = match get_mete_card_state(&card)? {
        // YES! KalkGetränke app <3
        MeteCardState::Uuid(uuid) => Some(CardDetail::MeteUuid(uuid)),
        MeteCardState::ApplicationUnknown => None,
        MeteCardState::InvalidAnswer => None,
        MeteCardState::UnsupportedApplicationSelect => {
            get_uid(&card)?.map(|uid| CardDetail::Plain(uid))
        }
    };
    Ok(result)
}

#[derive(Debug)]
pub enum CardDetail {
    MeteUuid(String),
    Plain(Vec<u8>),
}

#[derive(Default)]
struct Service {
    ctx: Option<Context>,
    reader_states: Vec<ReaderState>,
    readers_buf: Vec<u8>,
    reconnect_timeout: Duration,
}

impl Service {
    pub fn new() -> Service {
        Service {
            reader_states: vec![
                // Listen for reader insertions/removals, if supported.
                ReaderState::new(PNP_NOTIFICATION(), State::UNAWARE),
            ],
            readers_buf: vec![0; 2048],
            ..Default::default()
        }
    }

    fn get_context(&mut self) -> Result<Context, Box<dyn StdError>> {
        let ctx = match self.ctx.take() {
            Some(ctx) => ctx,
            None => {
                thread::sleep(self.reconnect_timeout);
                match Context::establish(Scope::User) {
                    Ok(ctx) => {
                        self.reconnect_timeout = Duration::from_secs(0);
                        ctx
                    }
                    Err(e) => {
                        self.reconnect_timeout = match self.reconnect_timeout.as_secs() {
                            0 => Duration::from_secs(1),
                            d => {
                                let mut d = d * d;
                                if d > 64 {
                                    d = 64;
                                }
                                Duration::from_secs(d)
                            }
                        };
                        return Err(Box::new(e));
                    }
                }
            }
        };
        Ok(ctx)
    }

    fn fetch_next_uuid_with_context(
        &mut self,
        ctx: Context,
    ) -> Result<Option<CardDetail>, Box<dyn StdError>> {
        loop {
            self.reader_states.retain(|rs| !is_dead(rs));

            let readers = ctx.list_readers(&mut self.readers_buf)?;

            for reader in readers {
                if !self.reader_states.iter().any(|rs| rs.name() == reader) {
                    self.reader_states
                        .push(ReaderState::new(reader, State::UNAWARE));
                }
            }

            // Update the view of the state to wait on.
            for rs in self.reader_states.iter_mut() {
                rs.sync_current_state();
            }
            ctx.get_status_change(None, &mut self.reader_states)?;

            let readers = ctx.list_readers(&mut self.readers_buf)?;
            let mut found_card = false;
            for reader in readers {
                match ctx.connect(reader, ShareMode::Shared, Protocols::ANY) {
                    Ok(card) => {
                        found_card = true;
                        match parse_card(card)? {
                            Some(carddetail) => return Ok(Some(carddetail)),
                            _ => continue,
                        }
                    }
                    Err(Error::NoSmartcard) => {
                        debug!("A smartcard is not present in the reader.");
                        continue;
                    }
                    Err(err) => return Err(err.into()),
                };
            }
            if found_card {
                return Ok(None);
            }
        }
    }

    pub fn fetch_next_uuid(&mut self) -> Result<Option<CardDetail>, Box<dyn StdError>> {
        let ctx = self.get_context()?;

        self.fetch_next_uuid_with_context(ctx)
    }
}

pub fn run() -> Result<impl Stream<Item = Option<CardDetail>>, Box<dyn StdError>> {
    let (tx, mut rx) = mpsc::channel(16);
    // hmmmm ... creating a new one seems wrong?
    let rt = tokio::runtime::Runtime::new()?;
    thread::spawn(move || {
        let mut service = Service::new();
        loop {
            match service.fetch_next_uuid() {
                Ok(result) => rt.block_on(async {
                    tx.send(result).await.unwrap();
                }),
                Err(e) => error!("Nfc Error: {:?}", e),
            }
        }
    });

    Ok(stream! {
        while let Some(uuid) = rx.recv().await {
            yield uuid;
        }
    })
}
