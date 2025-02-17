use anyhow::{anyhow, Context, Result};
use blake3::Hasher;
use futures_util::stream::StreamExt;
use hmac::Mac;
use rand::Rng;
use std::{io::Cursor, sync::Arc};
use std::net::{SocketAddr};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    select,
};
use tokio_util::codec::Decoder;
use tracing::debug;
mod config; // 声明模块

use crate::{
    args::{Opt, Script},
    client_hello::ClientHello,
    client_key_exchange::ClientKeyExchange,
    common::{
        CCS_RECORD, CLIENT_AUTH_SESSION_TICKET_LEN, CLIENT_AUTH_SESSION_TICKET_OFFSET,
        HANDSHAKE_TYPE_SERVER_KEY_EXCHANGE, RECORD_ALERT, RECORD_APPLICATION_DATA, RECORD_CCS,
        RECORD_HANDSHAKE, RESTLS_APPDATA_HMAC_LEN, RESTLS_APPDATA_LEN_OFFSET,
        RESTLS_APPDATA_OFFSET, RESTLS_HANDSHAKE_HMAC_LEN, RESTLS_MASK_LEN, TLS_RECORD_HEADER_LEN,
        TO_CLIENT_MAGIC, TO_SERVER_MAGIC,
    },
    server_hello::ServerHello,
    utils::{
        copy_bidirectional_fallback, tcp_rst, xor_bytes, DoubleCursorBuf, HandshakeRecord,
        RestlsCommand, TLSCodec, TLSStream,
    },
};

#[derive(Debug)]
enum TLS12Flow {
    Initial,
    CKEVerified,
    FullHandshakeClientCCS,
    ResumeClientCCS,
    FullHandshakeServerCCS,
    FullHandshakeServerFinished,
    ResumeServerFinished,
    ResumeServerCCS,
    Client0x17,
}

impl TLS12Flow {
    fn ccs_from_client(&mut self) -> Result<()> {
        match self {
            TLS12Flow::CKEVerified => {
                *self = TLS12Flow::FullHandshakeClientCCS;
                Ok(())
            }
            TLS12Flow::ResumeServerFinished => {
                *self = TLS12Flow::ResumeClientCCS;
                Ok(())
            }
            _ => Err(anyhow!(
                "reject: invalid flow, expect CKEVerified or ResumeServerCCS, actual: {:?}",
                self
            )),
        }
    }

    fn ccs_from_server(&mut self) -> Result<()> {
        match self {
            TLS12Flow::Initial => {
                *self = TLS12Flow::ResumeServerCCS;
                Ok(())
            }
            TLS12Flow::FullHandshakeClientCCS => {
                *self = TLS12Flow::FullHandshakeServerCCS;
                Ok(())
            }
            _ => Err(anyhow!(
                "reject: invalid flow, expect Initial or FullHandshakeClientCCS, actual: {:?}",
                self
            )),
        }
    }

    fn cke_verified(&mut self) -> Result<()> {
        match self {
            TLS12Flow::Initial => {
                *self = TLS12Flow::CKEVerified;
                Ok(())
            }
            _ => Err(anyhow!(
                "reject: invalid flow, expect Initial, actual: {:?}",
                self
            )),
        }
    }

    fn client_0x17(&mut self) -> Result<()> {
        use TLS12Flow::*;
        match self {
            FullHandshakeServerFinished | ResumeClientCCS => {
                *self = Client0x17;
                Ok(())
            }
            _ => Err(anyhow!(
                "reject: invalid flow, expect FullHandshakeServerFinished | ResumeClientCCS, actual: {:?}",
                self
            )),
        }
    }

    fn is_server_finished(&self) -> bool {
        match self {
            TLS12Flow::ResumeServerFinished | TLS12Flow::FullHandshakeServerFinished => true,
            _ => false,
        }
    }

    fn is_resume_ccs_from_client(&self) -> bool {
        match self {
            TLS12Flow::ResumeClientCCS => true,
            _ => false,
        }
    }

    fn is_resume(&self) -> bool {
        use TLS12Flow::*;

        match self {
            ResumeClientCCS | ResumeServerCCS => true,
            _ => false,
        }
    }

    fn expect_cke(&self) -> bool {
        match self {
            TLS12Flow::Initial => true,
            _ => false,
        }
    }

    fn is_client_0x17(&self) -> bool {
        match self {
            TLS12Flow::Client0x17 => true,
            _ => false,
        }
    }

    fn server_0x16(&mut self) -> Result<()> {
        use TLS12Flow::*;
        debug!("server_0x16");
        match self {
            FullHandshakeServerCCS => {
                *self = FullHandshakeServerFinished;
                Ok(())
            }
            ResumeServerCCS => {
                *self = ResumeServerFinished;
                Ok(())
            }
            FullHandshakeServerFinished | ResumeServerFinished => Err(anyhow!(
                "reject: there should be no 0x16 from server after ServerFinished"
            )),
            _ => Ok(()),
        }
    }
}

enum WriteToServerResult {
    Ok((u8, u8)),
    MaybeCloseNotify,
}

pub struct RestlsState<'a> {
    client_hello: Option<ClientHello>,
    server_hello: Option<ServerHello>,
    curve_id: Option<usize>,
    client_finished: Vec<u8>,
    restls_password: &'a [u8; 32],
    to_client_counter: u64,
    to_server_counter: u64,
    script: &'a Script,
    min_record_len: usize,
    parrot_tls12_gcm: bool,
    id: usize,
    client_ip: SocketAddr,
    server_port: &'a String,
}

fn sample_slice(data: &[u8]) -> &[u8] {
    &data[..std::cmp::min(32, data.len())]
}

impl<'a> RestlsState<'a> {
    fn restls_hmac(&self) -> Hasher {
        Hasher::new_keyed(self.restls_password)
    }

    pub fn restls_appdata_auth_hmac(&self, is_to_client: bool) -> Hasher {
        let mut hasher = self.restls_hmac();
        hasher.update(&self.server_hello.as_ref().unwrap().server_random);
        if is_to_client {
            hasher.update(TO_CLIENT_MAGIC);
            hasher.update(&self.to_client_counter.to_be_bytes());
        } else {
            hasher.update(TO_SERVER_MAGIC);
            hasher.update(&self.to_server_counter.to_be_bytes());
        }
        hasher
    }

    #[inline]
    fn restls_data_offset(&self, to_client: bool) -> usize {
        self.restls_header_offset(to_client) + RESTLS_APPDATA_OFFSET
    }

    #[inline]
    fn restls_header_offset(&self, to_client: bool) -> usize {
        TLS_RECORD_HEADER_LEN
            + if !to_client && self.server_hello.as_ref().unwrap().is_tls12_gcm
                || to_client && self.parrot_tls12_gcm
            {
                8
            } else {
                0
            }
    }

    pub fn read_app_data<'b>(&mut self, record: &'b mut [u8]) -> Result<(&'b [u8], RestlsCommand)> {
        let is_tls12_gcm = self.server_hello.as_ref().unwrap().is_tls12_gcm;

        if record.len() < self.restls_data_offset(false) {
            return Err(anyhow!(
                "[{}]reject: restls application data isn't long enough",
                self.id
            ));
        }
        if &record[..3] != &[RECORD_APPLICATION_DATA, 0x03, 0x03] {
            return Err(anyhow!(
                "[{}]reject: restls application data must have 0x17 header, got: {:?}",
                self.id,
                record
            ));
        }
        if is_tls12_gcm {
            let to_server_counter = u64::from_be_bytes(
                record[TLS_RECORD_HEADER_LEN..TLS_RECORD_HEADER_LEN + 8]
                    .try_into()?,
            );
            if to_server_counter != self.to_server_counter + 1 {
                return Err(anyhow!(
                    "[{}]reject: invalid to server counter for tls 1.2 aes-gcm, expect {}, actual {}",
                    self.id,
                    self.to_server_counter + 1, to_server_counter
                ));
            }
        }
        let mut hmac_auth = self.restls_appdata_auth_hmac(false);
        if self.client_finished.len() > 0 {
            debug!("adding client_finished {:?}", self.client_finished);
            hmac_auth.update(&self.client_finished);
            self.client_finished.resize(0, 0);
        }
        let header_offset = self.restls_header_offset(false);
        hmac_auth.update(&record[..header_offset]);
        let record = &mut record[header_offset..];
        let actual_auth = &record[..RESTLS_APPDATA_HMAC_LEN];
        hmac_auth.update(&record[RESTLS_APPDATA_LEN_OFFSET..]);
        let expect_auth = hmac_auth.finalize().into_bytes();
        if actual_auth != &expect_auth[..RESTLS_APPDATA_HMAC_LEN] {
            debug!(
                "[{}]bad mac record, expect auth {:?}, actual {:?}, to_client: {}, to_server: {}",
                self.id,
                &expect_auth[..RESTLS_APPDATA_HMAC_LEN],
                actual_auth,
                self.to_client_counter,
                self.to_server_counter,
            );
            return Err(anyhow!("reject: bad mac record"));
        }

        let mut hmac_mask = self.restls_appdata_auth_hmac(false);
        hmac_mask.update(sample_slice(&record[RESTLS_APPDATA_OFFSET..]));
        let mask = hmac_mask.finalize().into_bytes();
        let masked_section = &mut record[RESTLS_APPDATA_LEN_OFFSET..][..RESTLS_MASK_LEN];
        xor_bytes(&mask[..RESTLS_MASK_LEN], masked_section);
        let data_len = (masked_section[0] as usize) << 8 | (masked_section[1] as usize);
        let command = RestlsCommand::from_bytes(&masked_section[2..]);
        self.to_server_counter += 1;
        debug!("[{}]read_app_data: data_len {}", self.id, data_len);
        Ok((&record[RESTLS_APPDATA_OFFSET..][..data_len], command))
    }

    fn act_according_to_script(&self, data_len: usize) -> (usize, usize, RestlsCommand) {
        let line = self.script.get_line(self.to_client_counter as usize);
        let min_record_len = self.min_record_len + rand::thread_rng().gen_range(0..100);
        let (real_data_len, padding) = match (data_len < min_record_len, line) {
            (_, Some(line)) => {
                let target_len = line.len();
                if target_len < data_len {
                    (target_len, 0)
                } else {
                    (data_len, target_len - data_len)
                }
            }
            (true, None) => (data_len, min_record_len - data_len),
            (false, None) => (data_len, 0),
        };
        let command = match line {
            Some(line) => line.command,
            None => RestlsCommand::Noop,
        };
        (real_data_len, padding, command)
    }

    fn parrot_tls12_nonce(&self, record: &mut [u8]) {
        if self.parrot_tls12_gcm {
            debug!(
                "parrot gcm nounce {:?} {}",
                &record[5..13],
                self.to_client_counter + 1
            );
            record[5..13].copy_from_slice(&(self.to_client_counter + 1).to_be_bytes());
        }
    }

    fn prepare_app_data_header(
        &mut self,
        out_buf: &mut DoubleCursorBuf,
        data_len: usize,
        command: RestlsCommand,
    ) {
        let record = out_buf.load_mut();

        record[0..3].copy_from_slice(&[0x17, 0x3, 0x3]);
        let payload_len = (record.len() - 5) as u16;
        record[3..5].copy_from_slice(&payload_len.to_be_bytes());

        self.parrot_tls12_nonce(record);
        let mut hmac_auth = self.restls_appdata_auth_hmac(true);
        let header_offset = self.restls_header_offset(true);
        hmac_auth.update(&record[..header_offset]);
        let record = &mut record[header_offset..];

        let mut hmac_mask = self.restls_appdata_auth_hmac(true);
        hmac_mask.update(sample_slice(&record[RESTLS_APPDATA_OFFSET..]));
        let mask = hmac_mask.finalize().into_bytes();
        record[RESTLS_APPDATA_LEN_OFFSET..][..2].copy_from_slice(&(data_len as u16).to_be_bytes());

        record[RESTLS_APPDATA_LEN_OFFSET + 2..][..2].copy_from_slice(&command.to_bytes());
        xor_bytes(
            &mask[..RESTLS_MASK_LEN],
            &mut record[RESTLS_APPDATA_LEN_OFFSET..],
        );

        hmac_auth.update(&record[RESTLS_APPDATA_LEN_OFFSET..]);
        let auth = hmac_auth.finalize().into_bytes();
        record[..RESTLS_APPDATA_HMAC_LEN].copy_from_slice(&auth[..RESTLS_APPDATA_HMAC_LEN]);
        debug!(
            "[{}]write_header: data_len {}, padding: {}, mask: {:?}, auth: {:?}
            to_server {}, to_client {}",
            self.id,
            data_len,
            record.len() - data_len,
            &mask[..RESTLS_MASK_LEN],
            &auth[..RESTLS_APPDATA_HMAC_LEN],
            self.to_server_counter,
            self.to_client_counter,
        );
        self.to_client_counter += 1;
    }

    async fn read_from_stream(&self, stream: &mut TLSStream, eof_pending: bool) -> Result<()> {
        if stream.codec().has_next() {
            Ok(())
        } else {
            match stream.next().await {
                None => {
                    if eof_pending {
                        std::future::pending().await
                    } else {
                        Err(anyhow!("unexpected eof"))
                    }
                }
                Some(res) => res,
            }
        }
    }

    async fn try_read_client_hello(&mut self, inbound: &mut TLSStream) -> Result<()> {
        match self.read_from_stream(inbound, false).await {
            Err(e) => return Err(anyhow!("failed to read client hello: {}", e)),
            Ok(()) => (),
        };
        let rtype = inbound.codec().peek_record_type()?;
        if rtype != RECORD_HANDSHAKE {
            return Err(anyhow!(
                "reject: incorrect record type for client hello, actual: {}",
                rtype
            ));
        }
        let record = inbound
            .codec_mut()
            .next_record()
            .expect("unexpected error: record has been checked");
        let mut cursor = Cursor::new(&*record);
        self.client_hello = Some(
            ClientHello::parse(&mut cursor, self.id, self.client_ip).context("unable to parse client hello: ")?,
        );
        Ok(())
    }

    async fn try_read_server_hello(&mut self, outbound: &mut TLSStream) -> Result<()> {
        self.read_from_stream(outbound, false)
            .await
            .context("failed to read server hello: ")?;
        let rtype = outbound.codec().peek_record_type()?;
        if rtype != RECORD_HANDSHAKE {
            return Err(anyhow!(
                "reject: incorrect record type for server hello, actual: {}",
                rtype
            ));
        }
        let mut record = HandshakeRecord::new(
            outbound
                .codec_mut()
                .peek_record_mut()
                .expect("unexpected error: record has been checked"),
        );
        let mut cursor = Cursor::new(&*record.next_handshake_message()?);
        self.server_hello =
            Some(ServerHello::parse(&mut cursor).context("unable to parse client hello: ")?);
        if !record.has_next() {
            outbound.codec_mut().next_record()?;
        }
        Ok(())
    }

    async fn try_read_tls13_till_first_0x17(
        &mut self,
        outbound: &mut TLSStream,
        inbound: &mut TLSStream,
    ) -> Result<()> {
        let mut ccs_from_server = false;
        loop {
            self.read_from_stream(outbound, false).await?;
            let rtype = outbound.codec().peek_record_type()?;

            match rtype {
                RECORD_CCS if !ccs_from_server => {
                    ccs_from_server = true;
                }
                RECORD_APPLICATION_DATA if ccs_from_server => {
                    break;
                }
                _ => {
                    return Err(anyhow!(
                    "reject: incorrect outbound tls13 record type, expected 1 CCS or Application Data, actual {rtype}",
                ))
                }
            }
            outbound
                .codec_mut()
                .next_record()
                .expect("unexpected error: record has been checked");
            self.relay_to(inbound, outbound).await?;
        }

        Ok(())
    }

    fn prepare_server_auth(&mut self, outbound: &mut TLSStream) {
        let mut hasher = self.restls_hmac();
        hasher.update(&self.server_hello.as_ref().unwrap().server_random);
        let secret = hasher.finalize().into_bytes();
        debug!(
            "[{}]server challenge {:?}",
            self.id,
            &secret[..RESTLS_HANDSHAKE_HMAC_LEN]
        );
        let record = outbound
            .codec_mut()
            .peek_record_mut()
            .expect("unexpected error: record has been checked");
        let mut offset = 5;
        if self.server_hello.as_ref().unwrap().is_tls12_gcm {
            if u64::from_be_bytes(record[5..13].try_into().unwrap()) == 0 {
                offset = 13;
                self.parrot_tls12_gcm = true;
                debug!("[{}]parrot tls12 gcm", self.id);
            }
        }
        debug!("[{}]original server auth {:?}", self.id, record);
        xor_bytes(&secret[..RESTLS_HANDSHAKE_HMAC_LEN], &mut record[offset..]);
    }

    async fn try_read_tl13_till_client_application_data(
        &mut self,
        outbound: &mut TLSStream,
        inbound: &mut TLSStream,
    ) -> Result<()> {
        let mut seen_client_application_data = 0;
        let mut ccs_from_client = false;
        loop {
            select! {
                res = self.read_from_stream(inbound, false) => {
                    let _ = res.context("try_read_tl13_till_client_application_data inbound: ")?;
                    match inbound.codec().peek_record_type()? {
                        RECORD_CCS if !ccs_from_client => {
                            if inbound.codec().peek_record()? != CCS_RECORD {
                                return Err(anyhow!(
                                    "reject: tls13 incorrect CCS record from client",
                                ));
                            }
                            ccs_from_client = true;
                        }
                        RECORD_APPLICATION_DATA if ccs_from_client => {
                            seen_client_application_data += 1;
                            if seen_client_application_data == 1 {
                                self.client_finished.extend_from_slice(inbound.codec().peek_record()?);
                            } else if seen_client_application_data == 2 {
                                break;
                            }
                        }
                        rtype => {
                            return Err(anyhow!(
                                "reject: incorrect inbound tls13 record type, expected 1 CCS or Application Data, actual {rtype}",
                            ));
                        }
                    }
                    inbound.codec_mut().next_record().expect("unexpected error: record has been checked");
                    self.relay_to(outbound, inbound).await?;
                }
                res = self.read_from_stream(outbound, false) => {
                    let _ = res.context("try_read_tl13_till_client_application_data outbound: ")?;
                    outbound.codec_mut().next_record()?;
                    if seen_client_application_data == 1 {
                        self.to_client_counter += 1;
                    }
                    self.relay_to(inbound, outbound).await?;
                }
            }
        }
        Ok(())
    }

    async fn check_tls13_session_id(&mut self) -> Result<()> {
        let mut hasher = self.restls_hmac();
        let client_hello = self.client_hello.as_ref().unwrap();
        hasher.update(&client_hello.key_share);
        hasher.update(&client_hello.psk);
        let res_raw = hasher.finalize();
        let res = res_raw.into_bytes();
        let expect = &res[..RESTLS_HANDSHAKE_HMAC_LEN];
        let actual = &client_hello.session_id[..RESTLS_HANDSHAKE_HMAC_LEN];
        // we don't need constant time comparison since
        // that's not noticeable compared to network delay.
        let ip_str = self.client_ip.ip();
        if expect == actual {
            let mut logged_ips = config::LOGGED_IPS.lock().await; // 获取锁
            if logged_ips.insert(ip_str) { // 如果 insert 返回 true，表示之前没有记录过
                println!("Auth Success: Client ip: {:?}, Listen: {:?}", ip_str, self.server_port)
            }
            Ok(())
        } else {
            Err(anyhow!(
                "reject: incorrect session id, Client ip: {:?}, expect: {:?}, actual {:?}",
                ip_str,
                expect,
                actual
            ))
        }
    }

    fn handle_tls12_plaintext_handshake_msg(&mut self, record: &mut [u8]) {
        let mut hs = HandshakeRecord::new(record);
        while hs.has_next() {
            let msg = hs.next_handshake_message().unwrap();
            if msg[0] == HANDSHAKE_TYPE_SERVER_KEY_EXCHANGE && msg[4] == 3 {
                let curve_id_u16 = u16::from_be_bytes([msg[5], msg[6]]);
                self.curve_id = Some(curve_id_u16 as usize);
            } else if msg[0] == HANDSHAKE_TYPE_SERVER_KEY_EXCHANGE {
                debug!("[{}]curve not available {:?}", self.id, &msg[4..6]);
            }
        }
    }

    fn handle_tls12_outbound(
        &mut self,
        outbound: &mut TLSStream,
        flow: &mut TLS12Flow,
    ) -> Result<()> {
        debug!("handle_tls12_outbound");
        let rtype = outbound.codec().peek_record_type()?;
        match rtype {
            RECORD_CCS => {
                flow.ccs_from_server()?;
            }
            RECORD_HANDSHAKE => {
                flow.server_0x16()?;
                if flow.is_server_finished() {
                    if flow.is_resume() {
                        self.check_tls12_session_ticket()?;
                    }
                    self.prepare_server_auth(outbound);
                    debug!("[{}]sending tls12 server auth to client", self.id);
                    return Ok(());
                }
                let record = outbound.codec_mut().peek_record_mut()?;
                self.handle_tls12_plaintext_handshake_msg(record);
            }
            RECORD_APPLICATION_DATA if flow.is_server_finished() => {
                self.to_client_counter += 1;
            },
            _ => return Err(anyhow!("reject: incorrect outbound tls12 record type, expected 1 CCS or Handshake, actual {rtype}",)),
        }
        Ok(())
    }

    fn handle_tls12_inbound(&mut self, inbound: &TLSStream, flow: &mut TLS12Flow) -> Result<()> {
        debug!("handle_tls12_inbound");
        let rtype = inbound.codec().peek_record_type()?;
        match rtype {
            RECORD_CCS => {
                if inbound.codec().peek_record()? != CCS_RECORD {
                    return Err(anyhow!(
                        "reject: tls12 incorrect CCS record from client",
                    ));
                }
                flow.ccs_from_client()?;
                Ok(())
            }
            RECORD_HANDSHAKE if flow.expect_cke() => {
                // We expect CKE to be the first 0x16 after client hello.
                let maybe_cke = inbound
                    .codec()
                    .peek_record()
                    .expect("unexpected error: record has been checked");
                if self.curve_id.is_none() {
                    debug!("[{}]session_ticket: {:?}",self.id, self.client_hello.as_ref().unwrap().session_ticket);
                    return Err(anyhow!("reject: curve_id is not set"));
                }
                ClientKeyExchange::check(
                    &mut Cursor::new(maybe_cke),
                    self.client_hello.as_ref().unwrap(),
                    self.curve_id.unwrap(),
                    self.restls_hmac(),
                )?;
                flow.cke_verified()
            }
            RECORD_HANDSHAKE if flow.is_resume_ccs_from_client() => {
                // For tls 1.2 w/ resume, we need to hash the client finished.
                self.client_finished
                    .extend_from_slice(inbound.codec().peek_record()?);

                Ok(())
            }
            RECORD_HANDSHAKE => Ok(()),
            RECORD_APPLICATION_DATA => flow.client_0x17(),
            _ => Err(anyhow!(
                "reject: incorrect tls12 inbound record type, expected 1 CCS or Handshake, actual {rtype}",
            )),
        }
    }

    async fn try_read_tls12_till_client_application_data(
        &mut self,
        outbound: &mut TLSStream,
        inbound: &mut TLSStream,
    ) -> Result<()> {
        let mut flow = TLS12Flow::Initial;
        loop {
            debug!(
                "[{}]flow {:?}, outbound {}, inbound {}",
                self.id,
                flow,
                outbound.codec().has_next(),
                inbound.codec().has_next()
            );
            select! {
                ret = self.read_from_stream(outbound, false) => {
                    match ret {
                        Err(_) => return Ok(()),
                        _ => (),
                    }
                    self.handle_tls12_outbound(outbound, &mut flow)?;
                    outbound
                        .codec_mut()
                        .next_record()
                        .expect("unexpected error: record has been checked");
                    self.relay_to(inbound, outbound).await?;
                }
                ret = self.read_from_stream(inbound, false) => {
                    ret.context("try_read_tls12_till_client_application_data inbound: ")?;
                    self.handle_tls12_inbound(inbound, &mut flow)?;
                    if flow.is_client_0x17() {
                        break;
                    }
                    inbound
                        .codec_mut()
                        .next_record()
                        .expect("unexpected error: record has been checked");
                    self.relay_to(outbound, inbound).await?;
                }
            }
        }
        Ok(())
    }

    fn check_tls12_session_ticket(&self) -> Result<()> {
        debug!("checking tls12 session ticket");
        let mut hasher = self.restls_hmac();
        let client_hello = self.client_hello.as_ref().unwrap();
        hasher.update(&client_hello.session_ticket);
        let actual_hash = hasher.finalize().into_bytes();
        if &client_hello.session_id[CLIENT_AUTH_SESSION_TICKET_OFFSET..]
            != &actual_hash[..CLIENT_AUTH_SESSION_TICKET_LEN]
        {
            Err(anyhow!("reject: tls 1.2 session ticket mismatched"))
        } else {
            Ok(())
        }
    }

    async fn relay_to(
        &mut self,
        to_stream: &mut TLSStream,
        from_stream: &mut TLSStream,
    ) -> Result<()> {
        if from_stream.codec().has_next() {
            debug!("[{}]not relaying until data all read", self.id);
            return Ok(());
        }
        debug!("[{}]relay data", self.id);
        let res = match to_stream
            .get_mut()
            .write_all(&from_stream.codec().raw_buf())
            .await
        {
            Ok(()) => Ok(()),
            Err(e) => Err(e.into()),
        };
        from_stream.codec_mut().reset();
        res
    }

    fn prepare_packet_to_client(&mut self, out_buf: &mut DoubleCursorBuf) -> RestlsCommand {
        let (data_len, padding_len, command) = self.act_according_to_script(out_buf.len());
        out_buf.load(data_len + padding_len);
        self.prepare_app_data_header(out_buf, data_len, command);
        command
    }

    async fn write_server_data_to_client(
        &mut self,
        inbound: &mut TLSStream,
        out_buf: &mut DoubleCursorBuf,
    ) -> Result<(u8, bool)> {
        let mut write = 0;
        while out_buf.len() > 0 {
            let command = self.prepare_packet_to_client(out_buf);
            assert!(out_buf.load_mut().len() > 0);
            inbound
                .get_mut()
                .write_all(out_buf.load_mut())
                .await
                .context("inbound.write_all failed: ")?;
            out_buf.release();
            write += 1;
            match command {
                RestlsCommand::Response(awaiting) => return Ok((write, awaiting > 0)),
                _ => (),
            }
        }
        Ok((write, false))
    }

    async fn write_client_data_to_server(
        &mut self,
        inbound: &mut TLSStream,
        outbound: &mut TcpStream,
    ) -> Result<WriteToServerResult> {
        let mut need_respond = 0;
        let mut read_record = 0;
        while inbound.codec().has_next() {
            read_record += 1;
            let record = inbound.codec_mut().next_record()?;
            if record[0] == RECORD_ALERT {
                debug!("[{}]record[0] == RECORD_ALERT", self.id);
                return Ok(WriteToServerResult::MaybeCloseNotify);
            }
            let (record, command) = match self.read_app_data(record) {
                Ok(r) => r,
                Err(e) => {
                    return if self.server_hello.as_ref().unwrap().is_tls13
                        && (self.to_client_counter > 0 || self.to_server_counter > 0)
                    {
                        // this will probably be a close notify.
                        // we'll ignore it.
                        debug!("[{}]maybe close notify {}", self.id, e);
                        Ok(WriteToServerResult::MaybeCloseNotify)
                    } else {
                        Err(e)
                    };
                }
            };
            if record.len() > 0 {
                outbound
                    .write_all(record)
                    .await
                    .context("outbound.write_all failed: ")?;
            }
            match command {
                RestlsCommand::Noop => (),
                RestlsCommand::Response(count) => {
                    need_respond += count;
                }
            }
        }
        inbound.codec_mut().reset();
        Ok(WriteToServerResult::Ok((read_record, need_respond)))
    }

    async fn write_handshake_data_to_client(
        &mut self,
        inbound: &mut TLSStream,
        outbound_handshake: &mut TLSStream,
        close_notify_cache: &mut Vec<u8>,
    ) -> Result<()> {
        while outbound_handshake.codec().has_next() {
            let record = match outbound_handshake.codec_mut().next_record() {
                Ok(r) => r,
                Err(e) => {
                    debug!("[{}]outbound_handshake read error {}", self.id, e);
                    return Ok(());
                }
            };
            debug!("[{}]read from outbound_handshake {:?}", self.id, record);
            if record.len() < 50 {
                close_notify_cache.extend_from_slice(record);
                return Ok(());
            } else {
                self.parrot_tls12_nonce(record);
                inbound.get_mut().write_all(record).await?;
                self.to_client_counter += 1;
            }
        }
        Ok(())
    }

    async fn write_close_notify(&mut self, close_notify: &mut Vec<u8>, inbound: &mut TcpStream) {
        if close_notify.len() > 0 {
            debug!("[{}]sending close-notify {:?}", self.id, close_notify);
            self.parrot_tls12_nonce(close_notify);
            let _ = inbound.write(&close_notify).await;
        }
    }

    pub async fn copy_bidirectional(
        &mut self,
        inbound: &mut TLSStream,
        outbound: &mut TcpStream,
        outbound_handshake: &mut TLSStream,
    ) -> Result<()> {
        let mut out_buf = DoubleCursorBuf::new(self.restls_data_offset(true));
        let mut awaiting = false;
        let mut need_respond = 0;
        let mut close_notify = Vec::new();
        let mut outbound_handshake_closed = false;
        async fn read_if_has_capacity(
            outbound: &mut TcpStream,
            out_buf: &mut DoubleCursorBuf,
        ) -> Result<usize> {
            if out_buf.back_mut().len() > 0 {
                Ok(outbound.read(out_buf.back_mut()).await?)
            } else {
                std::future::pending().await
            }
        }
        loop {
            select! {
                res = self.read_from_stream(outbound_handshake, true) => {
                    debug!("[{}]read from outbound_handshake got {:?}", self.id, res);
                    self.write_handshake_data_to_client(inbound, outbound_handshake, &mut close_notify).await?;
                }
                res = self.read_from_stream(inbound, false) => {
                    res?;
                    match self.write_client_data_to_server(inbound, outbound).await? {
                        WriteToServerResult::MaybeCloseNotify => {
                            self.write_close_notify(&mut close_notify, inbound.get_mut()).await;
                            return Ok(())
                        },
                        WriteToServerResult::Ok((read, respond)) => {
                            awaiting = read == 0;
                            debug!("[{}]read {} and set awaiting to {}", self.id, read, awaiting);
                            need_respond += respond;
                        },
                    };
                }
                n = read_if_has_capacity(outbound, &mut out_buf) => {
                    let n = n.context("outbound.read failed: ")?;
                    if n == 0 {
                        self.write_close_notify(&mut close_notify, inbound.get_mut()).await;
                        return Ok(());
                    }
                    out_buf.advance_back(n);
                }
            }

            if (!awaiting || need_respond > 0) && out_buf.len() > 0 {
                debug!(
                    "[{}]writing to client: awaiting {}, need_respond {} ",
                    self.id, awaiting, need_respond
                );
                let (write, new_awaiting) = self
                    .write_server_data_to_client(inbound, &mut out_buf)
                    .await?;
                need_respond = if need_respond > write {
                    need_respond - write
                } else {
                    0
                };
                awaiting = new_awaiting;
                debug!(
                    "[{}]data set awaiting to {}, pending {}",
                    self.id,
                    new_awaiting,
                    out_buf.len()
                );
            }

            if need_respond > 0 {
                debug!("generating {} fake responses to client", need_respond);
                assert_eq!(out_buf.len(), 0);
                for _ in 0..need_respond {
                    let command = self.prepare_packet_to_client(&mut out_buf);
                    inbound
                        .get_mut()
                        .write_all(out_buf.load_mut())
                        .await
                        .context("write_client_data_to_server RestlsCommand::Response failed: ")?;
                    out_buf.release();
                    match command {
                        RestlsCommand::Response(count) => {
                            debug!("[{}]fake response set awaiting to true", self.id);
                            awaiting = count > 0;
                        }
                        _ => (),
                    }
                }
                need_respond = 0;
            }

            if !outbound_handshake_closed && self.to_client_counter + self.to_server_counter > 5 {
                outbound_handshake_closed = true;
                match outbound_handshake.get_mut().shutdown().await {
                    Err(e) => debug!("[{}]outbound_handshake shutdown got {}", self.id, e),
                    Ok(_) => (),
                }
            }
        }
    }

    async fn try_handshake(
        &mut self,
        outbound: &mut TLSStream,
        inbound: &mut TLSStream,
    ) -> Result<()> {
        self.try_read_client_hello(inbound).await?;
        self.relay_to(outbound, inbound).await?;

        self.try_read_server_hello(outbound).await?;
        self.relay_to(inbound, outbound).await?;

        if self.server_hello.as_ref().unwrap().is_tls13 {
            self.check_tls13_session_id().await?;
            self.try_read_tls13_till_first_0x17(outbound, inbound)
                .await?;
            self.prepare_server_auth(outbound);
            outbound.codec_mut().next_record()?;
            debug!("[{}]sending tls13 server auth to client", self.id);
            self.relay_to(inbound, outbound).await?;
            self.try_read_tl13_till_client_application_data(outbound, inbound)
                .await?;
        } else {
            self.try_read_tls12_till_client_application_data(outbound, inbound)
                .await?;
        }
        Ok(())
    }
}

pub async fn handle(options: Arc<Opt>, inbound: TcpStream, id: usize) -> Result<()> {
    let mut outbound = TLSCodec::new_inbound().framed(
        // TcpStream::connect("89.145.65.200:443")
        TcpStream::connect(&options.server_hostname)
            .await
            .context("cannot connect to outbound".to_owned() + &options.server_hostname)?,
    );
    let client_addr = inbound.peer_addr()?;
    let mut inbound = TLSCodec::new_outbound().framed(inbound);
    let mut try_handshake = RestlsState {
        client_hello: None,
        server_hello: None,
        curve_id: None,
        restls_password: &options.password.as_bytes(),
        client_finished: Vec::new(),
        to_client_counter: 0,
        to_server_counter: 0,
        script: &options.script,
        min_record_len: options.min_record_len as usize,
        parrot_tls12_gcm: false,
        id,
        client_ip: client_addr, // 将客户端地址传入 RestlsState
        server_port: &options.listen,
    };
    match try_handshake
        .try_handshake(&mut outbound, &mut inbound)
        .await
    {
        Ok(()) => {
            let mut outbound_proxy = TcpStream::connect(&options.forward_to).await?;
            match try_handshake
                .copy_bidirectional(&mut inbound, &mut outbound_proxy, &mut outbound)
                .await
            {
                Err(e) => {
                    tracing::error!("restls data relay failed({}): {}", client_addr.ip(), e);
                    tcp_rst(inbound.get_mut()).await?
                }
                _ => (),
            }
        }
        Err(e) => {
            tracing::error!("handshake failed({}): {:#}", client_addr.ip(), e);
            copy_bidirectional_fallback(inbound, outbound).await?;
        }
    }
    Ok(())
}

pub async fn start(options: Arc<Opt>) -> Result<()> {
    let listener = TcpListener::bind(&options.listen).await?;

    tracing::info!(
        "Restls server started as {} on {}, forwarding to {}",
        options.server_hostname,
        options.listen,
        options.forward_to,
    );
    let mut counter = 0;
    loop {
        let (stream, _) = listener
            .accept()
            .await
            .context("failed to accept inbound stream")?;
        let options = options.clone();
        stream.set_nodelay(true)?;

        tokio::spawn(async move {
            match handle(options, stream, counter).await {
                Err(e) => debug!("[{}]{}", counter, e),
                Ok(_) => debug!("[{}]closed", counter),
            }
        });
        counter += 1;
    }
}
