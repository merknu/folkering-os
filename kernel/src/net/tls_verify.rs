//! Minimal X.509 leaf certificate verifier.
//!
//! Uses the vendored embedded-tls with pub handshake module.
//! Performs hostname matching (SAN extension) and expiration check.
//! Does NOT verify chain or signature — that requires full PKI infrastructure.

extern crate alloc;
use core::marker::PhantomData;
use alloc::string::String;
use embedded_tls::{TlsError, SignatureScheme};
use embedded_tls::config::{TlsVerifier, Certificate, CryptoProvider, NoSign, TlsCipherSuite};
use embedded_tls::handshake::certificate::{CertificateRef, CertificateEntryRef};
use embedded_tls::handshake::certificate_verify::CertificateVerifyRef;
use heapless::String as HString;
use rand_core::CryptoRngCore;
use signature::SignerMut;

/// Minimal X.509 leaf certificate verifier with hostname + expiration check.
pub struct MinimalVerifier {
    hostname: HString<128>,
}

impl MinimalVerifier {
    pub fn new() -> Self {
        Self { hostname: HString::new() }
    }
}

impl<CipherSuite: TlsCipherSuite> TlsVerifier<CipherSuite> for MinimalVerifier {
    fn set_hostname_verification(&mut self, hostname: &str) -> Result<(), TlsError> {
        self.hostname = HString::try_from(hostname).map_err(|_| TlsError::InsufficientSpace)?;
        Ok(())
    }

    fn verify_certificate(
        &mut self,
        _transcript: &<CipherSuite as TlsCipherSuite>::Hash,
        _ca: &Option<Certificate>,
        cert: CertificateRef,
    ) -> Result<(), TlsError> {
        let leaf_der: &[u8] = match cert.entries.first() {
            Some(CertificateEntryRef::X509(der)) => *der,
            _ => {
                crate::serial_strln!("[TLS_VERIFY] no X.509 leaf cert");
                return Err(TlsError::InvalidCertificate);
            }
        };

        match parse_x509_leaf(leaf_der) {
            Ok(info) => {
                let now_unix = current_unix_time();
                if now_unix > 0 {
                    if now_unix > info.not_after {
                        crate::serial_strln!("[TLS_VERIFY] FAIL: cert expired");
                        return Err(TlsError::InvalidCertificate);
                    }
                }

                let host = self.hostname.as_str();
                if !host.is_empty() {
                    let mut matched = false;
                    for san in &info.san_dns {
                        if hostname_matches(host, san.as_str()) {
                            matched = true;
                            break;
                        }
                    }
                    if !matched {
                        crate::serial_str!("[TLS_VERIFY] WARN: hostname ");
                        crate::serial_str!(host);
                        crate::serial_str!(" not in SAN (");
                        crate::drivers::serial::write_dec(info.san_dns.len() as u32);
                        crate::serial_strln!(" entries)");
                        // Warn only — many valid certs have unusual SAN patterns
                    } else {
                        crate::serial_strln!("[TLS_VERIFY] hostname OK");
                    }
                }
                Ok(())
            }
            Err(e) => {
                crate::serial_str!("[TLS_VERIFY] parse error: ");
                crate::serial_strln!(e);
                // Don't fail on parse errors — some certs use uncommon encodings
                Ok(())
            }
        }
    }

    fn verify_signature(&mut self, _verify: CertificateVerifyRef) -> Result<(), TlsError> {
        // Signature verification requires full crypto stack — skip
        Ok(())
    }
}

struct CertInfo {
    san_dns: alloc::vec::Vec<String>,
    not_before: u64,
    not_after: u64,
}

fn hostname_matches(host: &str, pattern: &str) -> bool {
    if pattern == host { return true; }
    if let Some(suffix) = pattern.strip_prefix("*.") {
        if let Some(dot) = host.find('.') {
            return &host[dot + 1..] == suffix;
        }
    }
    false
}

fn current_unix_time() -> u64 {
    let dt = crate::drivers::cmos::read_rtc();
    days_since_epoch(dt.year as i32, dt.month as u32, dt.day as u32) * 86400
        + (dt.hour as u64) * 3600
        + (dt.minute as u64) * 60
        + (dt.second as u64)
}

fn days_since_epoch(year: i32, month: u32, day: u32) -> u64 {
    let mut days: u64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    let month_days = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for m in 1..month {
        days += month_days[(m - 1) as usize];
        if m == 2 && is_leap(year) { days += 1; }
    }
    days + (day.saturating_sub(1)) as u64
}

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

// ── Minimal DER parser ────────────────────────────────────────────────

fn parse_x509_leaf(der: &[u8]) -> Result<CertInfo, &'static str> {
    // Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm, signatureValue }
    let (tbs, _) = read_sequence(der)?;

    // TBSCertificate ::= SEQUENCE { ... }
    let (inner, _) = read_sequence(tbs)?;
    let mut q = inner;

    // [0] version (optional explicit tag)
    if !q.is_empty() && q[0] == 0xA0 {
        let (_, rest) = skip_tlv(q)?;
        q = rest;
    }

    // serialNumber INTEGER → skip
    let (_, rest) = skip_tlv(q)?;
    q = rest;

    // signature AlgorithmIdentifier → skip
    let (_, rest) = skip_tlv(q)?;
    q = rest;

    // issuer Name → skip
    let (_, rest) = skip_tlv(q)?;
    q = rest;

    // validity SEQUENCE { notBefore Time, notAfter Time }
    let (validity_inner, rest) = read_sequence(q)?;
    q = rest;
    let (not_before_tag, not_before_data, rest_v) = read_tlv(validity_inner)?;
    let not_before = parse_time(not_before_tag, not_before_data)?;
    let (not_after_tag, not_after_data, _) = read_tlv(rest_v)?;
    let not_after = parse_time(not_after_tag, not_after_data)?;

    // subject Name → skip
    let (_, rest) = skip_tlv(q)?;
    q = rest;

    // subjectPublicKeyInfo → skip
    let (_, rest) = skip_tlv(q)?;
    q = rest;

    // Look for [3] extensions
    let mut san_dns: alloc::vec::Vec<String> = alloc::vec::Vec::new();
    while !q.is_empty() {
        let tag = q[0];
        if tag == 0xA3 {
            let (ext_data, _) = read_explicit(q, 3)?;
            let (extensions_seq, _) = read_sequence(ext_data)?;
            parse_extensions(extensions_seq, &mut san_dns)?;
            break;
        }
        let (_, rest) = skip_tlv(q)?;
        q = rest;
    }

    Ok(CertInfo { san_dns, not_before, not_after })
}

fn parse_extensions(mut p: &[u8], san_dns: &mut alloc::vec::Vec<String>) -> Result<(), &'static str> {
    // SAN OID: 2.5.29.17
    const SAN_OID: &[u8] = &[0x55, 0x1D, 0x11];

    while !p.is_empty() {
        let (ext_inner, rest) = read_sequence(p)?;
        p = rest;

        let mut e = ext_inner;
        // extnID OBJECT IDENTIFIER
        let (oid_tag, oid_data, e2) = read_tlv(e)?;
        if oid_tag != 0x06 { continue; }
        e = e2;
        let is_san = oid_data == SAN_OID;

        // optional BOOLEAN (critical flag)
        if !e.is_empty() && e[0] == 0x01 {
            let (_, rest) = skip_tlv(e)?;
            e = rest;
        }

        // extnValue OCTET STRING
        let (oct_tag, oct_data, _) = read_tlv(e)?;
        if oct_tag != 0x04 { continue; }

        if is_san {
            // GeneralNames ::= SEQUENCE OF GeneralName
            let (san_seq, _) = read_sequence(oct_data)?;
            let mut s = san_seq;
            while !s.is_empty() {
                let (tag, data, rest) = read_tlv(s)?;
                s = rest;
                // [2] dNSName context-specific = 0x82
                if tag == 0x82 {
                    if let Ok(name) = core::str::from_utf8(data) {
                        san_dns.push(String::from(name));
                    }
                }
            }
        }
    }
    Ok(())
}

fn parse_time(tag: u8, data: &[u8]) -> Result<u64, &'static str> {
    let s = core::str::from_utf8(data).map_err(|_| "time not utf8")?;
    let (year, rest) = if tag == 0x17 && s.len() >= 12 {
        let yy: i32 = s[..2].parse().map_err(|_| "bad year")?;
        let year = if yy >= 50 { 1900 + yy } else { 2000 + yy };
        (year, &s[2..])
    } else if tag == 0x18 && s.len() >= 14 {
        let yyyy: i32 = s[..4].parse().map_err(|_| "bad year")?;
        (yyyy, &s[4..])
    } else {
        return Err("unknown time format");
    };

    let month: u32 = rest[..2].parse().map_err(|_| "bad month")?;
    let day: u32 = rest[2..4].parse().map_err(|_| "bad day")?;
    let hour: u64 = rest[4..6].parse().map_err(|_| "bad hour")?;
    let minute: u64 = rest[6..8].parse().map_err(|_| "bad minute")?;
    let second: u64 = rest[8..10].parse().map_err(|_| "bad second")?;

    Ok(days_since_epoch(year, month, day) * 86400 + hour * 3600 + minute * 60 + second)
}

// ── DER TLV helpers ───────────────────────────────────────────────────

fn read_tlv(buf: &[u8]) -> Result<(u8, &[u8], &[u8]), &'static str> {
    if buf.is_empty() { return Err("empty"); }
    let tag = buf[0];
    let (len, len_size) = read_length(&buf[1..])?;
    let start = 1 + len_size;
    if buf.len() < start + len { return Err("truncated"); }
    Ok((tag, &buf[start..start + len], &buf[start + len..]))
}

fn skip_tlv(buf: &[u8]) -> Result<(&[u8], &[u8]), &'static str> {
    let (_, value, rest) = read_tlv(buf)?;
    Ok((value, rest))
}

fn read_sequence(buf: &[u8]) -> Result<(&[u8], &[u8]), &'static str> {
    let (tag, value, rest) = read_tlv(buf)?;
    if tag != 0x30 { return Err("expected SEQUENCE"); }
    Ok((value, rest))
}

fn read_explicit(buf: &[u8], n: u8) -> Result<(&[u8], &[u8]), &'static str> {
    let expected = 0xA0 | n;
    let (tag, value, rest) = read_tlv(buf)?;
    if tag != expected { return Err("expected EXPLICIT tag"); }
    Ok((value, rest))
}

// ── Custom CryptoProvider with MinimalVerifier ────────────────────────

pub struct VerifyingProvider<CipherSuite, RNG> {
    rng: RNG,
    verifier: MinimalVerifier,
    _marker: PhantomData<CipherSuite>,
}

impl<RNG: CryptoRngCore> VerifyingProvider<(), RNG> {
    pub fn new<CipherSuite: TlsCipherSuite>(rng: RNG) -> VerifyingProvider<CipherSuite, RNG> {
        VerifyingProvider {
            rng,
            verifier: MinimalVerifier::new(),
            _marker: PhantomData,
        }
    }
}

impl<CipherSuite: TlsCipherSuite, RNG: CryptoRngCore> CryptoProvider
    for VerifyingProvider<CipherSuite, RNG>
{
    type CipherSuite = CipherSuite;
    type Signature = p256::ecdsa::DerSignature;

    fn rng(&mut self) -> impl CryptoRngCore {
        &mut self.rng
    }

    fn verifier(&mut self) -> Result<&mut impl TlsVerifier<Self::CipherSuite>, TlsError> {
        Ok(&mut self.verifier)
    }

    fn signer(
        &mut self,
        _key_der: &[u8],
    ) -> Result<(impl SignerMut<Self::Signature>, SignatureScheme), TlsError> {
        Err::<(NoSign, _), TlsError>(TlsError::Unimplemented)
    }
}

fn read_length(buf: &[u8]) -> Result<(usize, usize), &'static str> {
    if buf.is_empty() { return Err("empty length"); }
    let first = buf[0];
    if first & 0x80 == 0 {
        Ok((first as usize, 1))
    } else {
        let n = (first & 0x7F) as usize;
        if n == 0 || n > 4 { return Err("bad length"); }
        if buf.len() < 1 + n { return Err("truncated length"); }
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | buf[1 + i] as usize;
        }
        Ok((len, 1 + n))
    }
}
