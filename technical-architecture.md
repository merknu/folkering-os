# Teknisk Arkitektur - Folkering OS

## System-oversikt

```
┌─────────────────────────────────────────────────────────────┐
│  FOLKERING OS - SYSTEMARKITEKTUR                            │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  Layer 1: Hardware                                          │
│    ├─ TPM 2.0 Chip (nøkkellagring, PCR-binding)           │
│    └─ LUKS2-kryptert disk                                  │
│                                                             │
│  Layer 2: Boot & Init                                       │
│    ├─ UEFI Secure Boot                                     │
│    ├─ GRUB2 bootloader                                     │
│    ├─ Initramfs (systemd-cryptsetup)                      │
│    └─ systemd init                                         │
│                                                             │
│  Layer 3: Folkering Services                               │
│    ├─ folkering-auth-daemon (OIDC)                        │
│    ├─ folkering-tpm-kdf (TPM-integrasjon)                 │
│    ├─ folkering-cache-manager (offline)                   │
│    └─ folkering-pam (user sessions)                       │
│                                                             │
│  Layer 4: Desktop Environment                              │
│    ├─ Custom Display Manager (SDDM/GDM fork)              │
│    ├─ GNOME/KDE desktop (valgfritt)                       │
│    └─ Norske applikasjoner pre-installert                 │
│                                                             │
│  Layer 5: External Services                                │
│    ├─ BankID OIDC Provider                                 │
│    ├─ Feide (for skole/studenter)                         │
│    └─ Altinn, Helsenorge, osv (fremtidig)                 │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

---

## Detaljert komponentbeskrivelse

### 1. Custom Display Manager: `folkering-greeter`

**Ansvar**: Håndtere innloggingsskjerm med BankID/Feide

**Teknologi**: Fork av SDDM eller GDM

**Funksjonalitet**:
```
- Viser QR-kode for BankID-autentisering
- Fallback til tradisjonell passord-login
- Feide-valg for elever/studenter
- Offline-modus med cached credentials
- Tilgjengelighetsfunksjoner (skjermleser, høykontrast)
```

**Konfigurasjon**: `/etc/folkering/greeter.conf`

---

### 2. OIDC Authentication Service: `folkering-auth-daemon`

**Ansvar**: Håndtere OAuth2/OIDC-flyt med BankID

**Teknologi**: Python Flask + Authlib

**API Endpoints**:
```
GET  /login           - Start OIDC authorization flow
GET  /callback        - Motta authorization code
POST /token           - Exchange code for ID token
GET  /userinfo        - Hent brukerinfo
POST /logout          - Logg ut bruker
GET  /health          - Health check
```

**Dataflyt**:
```python
# 1. Authorization Request
GET https://auth.current.bankid.no/auth/realms/current/protocol/openid-connect/auth
  ?client_id=folkering-os
  &redirect_uri=http://localhost:8080/callback
  &response_type=code
  &scope=openid profile nnin
  &code_challenge=<sha256(verifier)>
  &code_challenge_method=S256
  &state=<random>

# 2. User authenticates on mobile (BankID app)

# 3. Callback received
GET http://localhost:8080/callback
  ?code=AUTH_CODE_123
  &state=<same_random>

# 4. Token Exchange
POST https://auth.current.bankid.no/auth/realms/current/protocol/openid-connect/token
  grant_type=authorization_code
  &code=AUTH_CODE_123
  &redirect_uri=http://localhost:8080/callback
  &client_id=folkering-os
  &client_secret=<FROM_TPM>
  &code_verifier=<original>

# 5. Receive ID Token (JWT)
{
  "iss": "https://auth.current.bankid.no/auth/realms/current",
  "sub": "abc123def456...",  # Hashed fødselsnummer
  "aud": "folkering-os",
  "exp": 1737470000,
  "iat": 1737466400,
  "name": "Ola Nordmann",
  "birthdate": "1990-05-15",
  "locale": "nb-NO"
}
```

**Sikkerhet**:
- PKCE (Proof Key for Code Exchange) obligatorisk
- `client_secret` lagres i TPM NVRAM
- JWT signature verification mot BankID JWKS
- State parameter for CSRF-beskyttelse

**Konfigurasjon**: `/etc/folkering/auth-daemon.conf`

---

### 3. TPM Key Derivation: `folkering-tpm-kdf`

**Ansvar**: Generere LUKS-nøkler fra BankID-identitet via TPM

**Teknologi**: Python + tpm2-pytss

**Nøkkel-hierarki**:
```
TPM Storage Root Key (SRK)
  └─ Folkering Parent Key (seeded ved installasjon)
      └─ User Key (per bruker, basert på BankID sub)
          └─ LUKS Master Key
```

**Derivasjon-algoritme**:
```python
def derive_luks_key(id_token_sub: str, device_uuid: str) -> bytes:
    """
    Deriverer LUKS-nøkkel fra BankID subject og device UUID

    Args:
        id_token_sub: BankID 'sub' claim (hashed fødselsnummer)
        device_uuid: Unik maskin-ID (/etc/machine-id)

    Returns:
        32-byte LUKS key
    """
    # 1. Kombiner identitet med maskin
    seed_material = f"{id_token_sub}:{device_uuid}".encode('utf-8')

    # 2. Seal med TPM (binding til PCRs)
    with ESAPI() as tpm:
        parent_key = tpm.load('/var/lib/folkering/tpm_parent.key')

        pcr_selection = TPML_PCR_SELECTION([
            TPMS_PCR_SELECTION(
                hash=TPM2_ALG_SHA256,
                select=[0, 2, 7, 14]  # Firmware, boot, SecureBoot, MOK
            )
        ])

        # Seal data - kan bare unseales med samme PCR-verdier
        sealed_data = tpm.create(
            parent=parent_key,
            inSensitive=TPM2B_SENSITIVE_CREATE(data=seed_material),
            creationPCR=pcr_selection
        )

        # Unseal (krever korrekt system state)
        master_seed = tpm.unseal(sealed_data)

    # 3. Derive LUKS key med PBKDF2
    salt = load_salt_from_disk()  # Persistent salt

    luks_key = PBKDF2HMAC(
        algorithm=hashes.SHA256(),
        length=32,
        salt=salt,
        iterations=100000
    ).derive(master_seed)

    return luks_key
```

**PCR-binding forklart**:
```
PCR 0:  UEFI firmware code
PCR 2:  UEFI drivers og applikasjoner
PCR 7:  Secure Boot state (certificates, keys)
PCR 14: MOK (Machine Owner Keys) for 3rd-party drivers

Hvis noen av disse endres (f.eks. firmware update),
vil TPM unseal feile → Krev recovery passord
```

**Re-sealing etter updates**:
```bash
# Etter firmware-oppdatering
folkering-tpm-reseal \
  --verify-user-password \
  --update-pcrs 0,2,7,14
```

---

### 4. Offline Cache Manager: `folkering-cache-manager`

**Ansvar**: Sikker offline-autentisering

**Lagringslokasjon**: `/var/lib/folkering/offline_cache`

**Datastruktur**:
```json
{
  "version": 1,
  "user_sub": "abc123def456...",
  "user_name": "Ola Nordmann",
  "cached_at": "2026-01-20T10:00:00Z",
  "valid_until": "2026-01-27T10:00:00Z",
  "master_seed_encrypted": "<base64>",
  "nonce": "<base64>",
  "tag": "<base64>"
}
```

**Kryptering**:
```python
def encrypt_cache(data: dict, tpm_key: bytes) -> bytes:
    """
    AES-256-GCM autentisert kryptering
    """
    from cryptography.hazmat.primitives.ciphers.aead import AESGCM

    aesgcm = AESGCM(tpm_key)
    nonce = os.urandom(12)  # 96-bit nonce

    plaintext = json.dumps(data).encode('utf-8')
    ciphertext = aesgcm.encrypt(nonce, plaintext, None)

    return {
        'ciphertext': base64.b64encode(ciphertext),
        'nonce': base64.b64encode(nonce)
    }

def decrypt_cache(encrypted: dict, tpm_key: bytes) -> dict:
    """
    Dekrypter og verifier integritet
    """
    aesgcm = AESGCM(tpm_key)

    ciphertext = base64.b64decode(encrypted['ciphertext'])
    nonce = base64.b64decode(encrypted['nonce'])

    try:
        plaintext = aesgcm.decrypt(nonce, ciphertext, None)
        return json.loads(plaintext)
    except InvalidTag:
        raise SecurityError("Cache tampered with!")
```

**Cache-oppdatering**:
```python
def update_cache_after_login(id_token: dict):
    """
    Kalles etter vellykket online-autentisering
    """
    tpm_cache_key = derive_cache_key_from_tpm()

    cache_data = {
        'user_sub': id_token['sub'],
        'user_name': id_token['name'],
        'cached_at': datetime.utcnow().isoformat(),
        'valid_until': (datetime.utcnow() + timedelta(days=7)).isoformat(),
        'master_seed_encrypted': encrypt_seed_with_tpm(master_seed)
    }

    encrypted = encrypt_cache(cache_data, tpm_cache_key)
    write_to_disk('/var/lib/folkering/offline_cache', encrypted)
```

**Offline-validering**:
```python
def validate_offline_login() -> bool:
    """
    Sjekk om offline-cache er gyldig
    """
    if not os.path.exists('/var/lib/folkering/offline_cache'):
        return False

    tpm_cache_key = derive_cache_key_from_tpm()
    cache = decrypt_cache(read_from_disk(), tpm_cache_key)

    valid_until = datetime.fromisoformat(cache['valid_until'])

    if datetime.utcnow() > valid_until:
        return False  # Utløpt

    return True
```

---

### 5. PAM Module: `pam_folkering.so`

**Ansvar**: Integrere Folkering-autentisering i Linux PAM-stacken

**Plassering**: `/usr/lib/security/pam_folkering.so`

**PAM-konfigurasjon** (`/etc/pam.d/folkering-greeter`):
```
#%PAM-1.0
auth       requisite    pam_nologin.so
auth       [success=2 default=ignore]  pam_folkering.so try_first_pass
auth       [success=1 default=ignore]  pam_unix.so nullok
auth       requisite    pam_deny.so
auth       required     pam_permit.so

account    required     pam_folkering.so
account    required     pam_unix.so

session    required     pam_limits.so
session    required     pam_unix.so
session    required     pam_folkering.so
```

**Funksjonalitet**:
```c
// Forenklet PAM-modul
PAM_EXTERN int pam_sm_authenticate(
    pam_handle_t *pamh,
    int flags,
    int argc,
    const char **argv
) {
    // 1. Sjekk om folkering-auth-daemon har ID token
    const char *id_token = get_current_id_token();

    if (!id_token) {
        // Fallback til tradisjonell autentisering
        return PAM_AUTHINFO_UNAVAIL;
    }

    // 2. Valider JWT
    if (!validate_jwt_signature(id_token)) {
        return PAM_AUTH_ERR;
    }

    // 3. Ekstraher brukeridentitet
    const char *subject = jwt_get_claim(id_token, "sub");
    const char *name = jwt_get_claim(id_token, "name");

    // 4. Opprett/finn bruker basert på subject
    struct passwd *pwd = get_user_by_bankid_sub(subject);

    if (!pwd) {
        // Første gangs innlogging - opprett bruker
        pwd = create_user_from_bankid(subject, name);
    }

    // 5. Sett PAM-variabler
    pam_set_item(pamh, PAM_USER, pwd->pw_name);
    pam_set_data(pamh, "folkering_id_token", strdup(id_token), cleanup);

    return PAM_SUCCESS;
}
```

---

## Sikkerhetsbetraktninger

### Trusselmodell

| Trussel | Mitigering |
|---------|-----------|
| **Fysisk disk-tyveri** | LUKS2-kryptering, TPM-binding |
| **Evil Maid attack** | PCR-binding, Secure Boot, TPM attestation |
| **Replay attack** | JWT exp claim, nonce i OIDC flow |
| **Man-in-the-middle** | HTTPS til BankID, certificate pinning |
| **Cache tampering** | AES-GCM autentisert kryptering |
| **TPM reset** | Recovery passord (LUKS keyslot 0) |
| **Firmware rootkit** | PCR-verdier endres → Unseal feiler |

### Defense in Depth

```
Layer 1: Secure Boot (signerte bootloadere)
Layer 2: TPM PCR-binding (trusted boot chain)
Layer 3: LUKS2 disk encryption
Layer 4: BankID OIDC (2FA på mobil)
Layer 5: Offline-cache expiry (7 dager)
Layer 6: Audit logging (systemd journal)
```

---

## Ytelse og skalerbarhet

### Boot-tid
```
Target: < 30 sekunder fra power-on til login-skjerm
- UEFI POST: ~5s
- GRUB: ~2s
- Kernel load: ~3s
- Initramfs (LUKS unlock): ~5s (venter på BankID)
- Systemd init: ~10s
- Display manager: ~5s
```

### BankID response tid
```
Typical: 10-20 sekunder (avhenger av bruker)
- QR-kode scan: ~2s
- BankID app open: ~1s
- User authentication (biometri): ~2s
- OIDC redirect: ~5-10s
```

### Offline-modus
```
Login-tid: < 5 sekunder
- Ingen nettverkskall
- TPM unseal: ~1s
- Cache decrypt: ~0.5s
- PAM session: ~3s
```

---

## Fremtidige forbedringer

### v1.1 - Feide-støtte
- Implementer SAML/OIDC med Feide
- Aldersverifisering (barn < 13 år)
- Foresatt-godkjenning

### v1.2 - Multi-device sync
- Roaming profiles via end-to-end encrypted sync
- Same identity, multiple machines
- Conflict resolution

### v2.0 - Økosystem-integrasjon
- Altinn API: Automatisk skattemelding-import
- Helsenorge: Sikker tilgang til pasientjournal
- Vipps: OS-nivå betalinger

### v3.0 - Nordisk ekspansjon
- Sverige: BankID + Feide ekvivalenter
- Danmark: MitID + UNI-Login
- Finland: FTN + Haka

---

**Versjon**: 0.1
**Sist oppdatert**: 2026-01-20
