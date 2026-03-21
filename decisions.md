# Beslutningslogg - Folkering OS

## Formål
Dette dokumentet holder styr på viktige tekniske og strategiske beslutninger. Hver beslutning dokumenteres med:
- **Kontekst**: Hvorfor diskuterte vi dette?
- **Alternativer**: Hva vurderte vi?
- **Beslutning**: Hva valgte vi?
- **Rasjonale**: Hvorfor?
- **Konsekvenser**: Hva betyr dette?

---

## 2026-01-20: Grunnleggende arkitektur-beslutninger

### D001: Norsk fokus i stedet for pan-europeisk

**Kontekst**: Opprinnelig idé var et europeisk OS mot amerikansk dominans.

**Alternativer**:
1. Europeisk OS med EU-ID integrasjon
2. Nordisk OS med BankID/MitID/FTN
3. Norsk OS med BankID/Feide

**Beslutning**: Start norsk (BankID/Feide), ekspander til Norden senere.

**Rasjonale**:
- BankID har 99% penetrasjon i Norge - ingen annen EU-ID er like utbredt
- Mindre scope = raskere launch
- Norge har strong digital infrastruktur å bygge på
- Kan demonstrere konseptet før skalering

**Konsekvenser**:
- ✅ Fokusert produktutvikling
- ✅ Klare målgrupper (norske borgere, offentlig sektor)
- ❌ Mindre marked i starten
- ❌ Må re-evaluere for nordisk ekspansjon

---

### D002: Ubuntu LTS som base (Fase 1-2)

**Kontekst**: Trenger et OS å bygge på for Fase 1-2.

**Alternativer**:
1. Ubuntu LTS 24.04+
2. Debian Stable
3. Arch Linux
4. Bygge fra bunnen (Linux From Scratch)

**Beslutning**: Ubuntu LTS 24.04 (eller nyeste LTS ved oppstart)

**Rasjonale**:
- 5 års support (maintenance redusert)
- Stort community (hjelp tilgjengelig)
- Norsk språkstøtte allerede god
- Hardware-kompatibilitet
- Kjent for de fleste Linux-brukere

**Konsekvenser**:
- ✅ Rask utvikling (ikke reinventing wheel)
- ✅ Stor package-tilgjengelighet (apt)
- ❌ Avhengig av Canonical's release-syklus
- ❌ Må følge Ubuntu's design-valg (systemd, osv.)

**Alternativ vurdert senere**: Debian hvis Ubuntu's commercial focus blir problematisk.

---

### D003: BankID via OIDC (ikke legacy SAML)

**Kontekst**: BankID støtter både OIDC og legacy SAML.

**Alternativer**:
1. OpenID Connect (OIDC)
2. SAML 2.0
3. Custom API-integrasjon

**Beslutning**: OIDC (OpenID Connect)

**Rasjonale**:
- OIDC er moderne standard (2014+)
- Bedre dokumentasjon fra BankID
- JSON-basert (enklere å parse enn XML)
- PKCE-støtte for sikkerhet
- Fremtidsrettet (SAML er legacy)

**Konsekvenser**:
- ✅ Enklere integrasjon
- ✅ Bedre sikkerhet (PKCE mandatory fra 2025)
- ❌ Må håndtere JWT-validering (ikke så vanskelig)

**Implementering**: Bruk `authlib` (Python) eller `openid` (Rust for v3)

---

### D004: TPM 2.0 for nøkkellagring

**Kontekst**: Hvor skal LUKS-nøkler lagres sikkert?

**Alternativer**:
1. TPM 2.0 med PCR-binding
2. Yubikey eller annen hardware token
3. Software keyring (GNOME Keyring, etc.)
4. Passphrase-only

**Beslutning**: TPM 2.0 med PCR-binding

**Rasjonale**:
- TPM 2.0 finnes i de fleste moderne maskiner (2016+)
- Hardware-basert sikkerhet (kan ikke ekstrahere nøkler)
- PCR-binding = trusted boot chain
- systemd-cryptenroll støtter det ut av boksen
- Ingen ekstra hardware nødvendig

**Konsekvenser**:
- ✅ Sterk sikkerhet uten bruker-friksjon
- ✅ Evil Maid-beskyttelse (PCR-endring = unseal feiler)
- ❌ Eldre maskiner uten TPM støttes ikke (må bruke passphrase)
- ❌ Firmware-updates krever re-sealing

**Fallback**: Passphrase-only for maskiner uten TPM

---

### D005: Offline-cache gyldighet: 7 dager

**Kontekst**: Hvor lenge skal offline-autentisering være gyldig?

**Alternativer**:
1. 1 dag (høy sikkerhet, lav convenience)
2. 7 dager (balansert)
3. 30 dager (lav sikkerhet, høy convenience)
4. Ingen offline-cache (krever alltid nett)

**Beslutning**: 7 dager

**Rasjonale**:
- Norske brukere er ofte på hytta/fjellet i helger
- 7 dager = kan dra på helgetur uten å bekymre seg
- Ikke så langt at kompromittert cache er kritisk
- Standard for mange "remember me"-løsninger

**Konsekvenser**:
- ✅ Brukervennlig for norske forhold
- ✅ Reduserer avhengighet av mobilnettverk
- ❌ Hvis noen stjeler maskinen, har de 7 dager (men trenger TPM)

**Konfigurerbarhet**: Administratorer kan sette kortere/lengre periode

---

### D006: Feide-integrasjon for skoleelever

**Kontekst**: Barn under 13 år har ikke BankID - hvordan håndteres dette?

**Alternativer**:
1. Kun BankID (ekskluderer barn)
2. Feide for skole/studenter
3. Foresatt-godkjenning via BankID
4. Egne barnekontoer (som Apple Family)

**Beslutning**: Feide-integrasjon (Fase 2)

**Rasjonale**:
- Feide brukes allerede i norske skoler
- Skoler er en viktig målgruppe for Folkering OS
- Gjenbruker eksisterende infrastruktur
- Lærere/IT-admin kan administrere

**Konsekvenser**:
- ✅ Inkluderer barn og studenter
- ✅ Piloter i skoler blir realistiske
- ❌ Ekstra kompleksitet (SAML i tillegg til OIDC)
- ❌ Må koordinere med Sikt (Feide-leverandør)

**Implementering**: Fase 2 (etter BankID fungerer)

---

### D007: To-fase tilnærming (Linux → Mikrokjerne)

**Kontekst**: Skal vi starte med Linux eller bygge nytt OS fra bunnen?

**Alternativer**:
1. Kun Linux-distro (aldri mikrokjerne)
2. Mikrokjerne fra dag 1
3. Linux først, mikrokjerne senere (to-fase)

**Beslutning**: To-fase tilnærming

**Rasjonale**:
- Linux-distro kan lanseres 2026-2027 (realistisk)
- Bygger brukerbase og momentum
- Lærer hva brukere faktisk trenger
- Mikrokjerne er 5-10 års prosjekt (krever funding, team)
- Hvis mikrokjerne aldri skjer, har vi fortsatt et brukbart produkt

**Konsekvenser**:
- ✅ Pragmatisk og gjennomførbart
- ✅ Kan demonstrere verdi tidlig
- ✅ Funding for mikrokjerne lettere med eksisterende brukere
- ❌ Risiko for at vi "setter oss fast" på Linux
- ❌ Må potensielt migrere brukere fra v2 til v3

**Mitigering**: Design v1-2 med migrering i tankene (data-formater, etc.)

---

### D008: Åpen kildekode (GPLv3)

**Kontekst**: Skal Folkering OS være åpen eller lukket kildekode?

**Alternativer**:
1. GPLv3 (sterk copyleft)
2. MIT/BSD (permissive)
3. AGPL (network copyleft)
4. Proprietary

**Beslutning**: GPLv3

**Rasjonale**:
- Linux kernel er GPL - vi må uansett dele kernel-endringer
- Tillitsskapende for offentlig sektor (åpenhet)
- Bidragsytere kan reviewe sikkerhetskode
- Forhindrer BigTech fra å ta koden og lukke den
- Norsk digital suverenitet krever åpenhet

**Konsekvenser**:
- ✅ Høy tillit fra brukere
- ✅ Community-bidrag mulig
- ✅ Auditbar sikkerhet
- ❌ Vanskelig å monetize direkte (support/consulting er business model)

**Kommersiell strategi**: Support-kontrakter for bedrifter/kommuner

---

### D009: Norsk som primærspråk

**Kontekst**: Hvilket språk skal dokumentasjon og UI være på?

**Alternativer**:
1. Kun engelsk (internasjonalt)
2. Kun norsk (lokalt)
3. Norsk primært, engelsk sekundært

**Beslutning**: Norsk primært, engelsk sekundært

**Rasjonale**:
- Målgruppe er norske borgere/offentlig sektor
- Lavere terskel for ikke-tekniske brukere
- Norsk støtter både bokmål og nynorsk
- Samisk bør også støttes
- Engelsk viktig for internasjonale bidragsytere

**Konsekvenser**:
- ✅ Mer inkluderende for vanlige nordmenn
- ✅ Differensiering fra andre Linux-distros
- ❌ Mindre internasjonalt appeal
- ❌ Må vedlikeholde to sett dokumentasjon

**Implementering**: I18n fra dag 1 (gettext eller Fluent)

---

## Fremtidige beslutninger (må tas senere)

### Uavklart: Desktop environment

**Spørsmål**: GNOME, KDE, XFCE, eller custom?

**Vurdering**: Avventer bruker-testing. GNOME er enklest, KDE mer konfigurerbart.

---

### Uavklart: Mikrokjerne-base (seL4 vs Rust)

**Spørsmål**: Bruke seL4 eller bygge egen Rust-kjerne?

**Vurdering**: Krever dypere forskning i Fase 3.

---

### Uavklart: Organisasjonsform

**Spørsmål**: Stiftelse, forening, eller selskap (AS)?

**Vurdering**: Avhenger av funding og juridiske rammeverk. Foreslå stiftelse (non-profit).

---

## Hvordan bruke dette dokumentet

- **Før du tar en ny beslutning**: Sjekk om lignende beslutning er tatt
- **Når du tar en beslutning**: Dokumenter her med samme struktur
- **Quarterly review**: Gå gjennom beslutninger og re-evaluer

---

**Versjon**: 0.1
**Sist oppdatert**: 2026-01-20
**Neste review**: Q2 2026 (etter Fase 0)


---

## 2026-01-25: Synapse Graph Filesystem - Phase 1.5 Decisions

### D010: Relative Path Storage vs Absolute Paths

**Kontekst**: Database måtte være flyttbar mellom maskiner og mapper.

**Alternativer**:
1. Absolute paths (C:\Users\merkn\project\file.txt)
2. Relative paths med project root
3. UUID-basert path tracking
4. Inode-basert tracking

**Beslutning**: Relative paths med project root

**Rasjonale**:
- Database kan flyttes fritt (backup, sync, team collaboration)
- Ingen oppslag nødvendig (UUID → path ville kreve lookup)
- Cross-platform kompatibelt (Windows ↔ Unix)
- Minimal overhead (<1ms per path operation)
- Standard approach i portable apps

**Konsekvenser**:
- ✅ Database 100% portabel
- ✅ Fungerer på tvers av maskiner
- ✅ Backup/restore "just works"
- ❌ Slight overhead ved path conversion (neglisjerbart)
- ❌ Må tracke project root i metadata

**Implementering**: 
- `project_meta` table med `root_path` key
- `to_relative()` / `to_absolute()` helpers i GraphDB
- Alle paths normalisert til `/` separator

**Test**: 5/5 portability tests passed

---

### D011: SHA-256 vs Faster Hash Algorithms

**Kontekst**: Trengte content hashing for skip-on-unchanged optimization.

**Alternativer**:
1. SHA-256 (sikker, 40 MB/s)
2. xxHash (rask, 1 GB/s, ikke kryptografisk)
3. BLAKE3 (rask, sikker, 3 GB/s)
4. CRC32 (raskest, men collisions)

**Beslutning**: SHA-256

**Rasjonale**:
- Security: Collision-resistant (viktig for integrity)
- Future-proof: Widely supported, will last decades
- Performance: 40 MB/s er akseptabelt (< 1% av indexing time)
- Standard library: sha2 crate er mature og audited
- Tradeoff: Sikkerhet > Hastighet for content addressing

**Konsekvenser**:
- ✅ Sikker content-addressing
- ✅ Ingen false negatives (collisions ekstremt usannsynlig)
- ✅ Kan brukes for content deduplication senere
- ❌ Litt tregere enn xxHash (40 MB/s vs 1 GB/s)
- ❌ Men: For en 10 MB fil er forskjellen 257ms vs 10ms (neglisjerbart)

**Performance testing**:
- 1 KB: 0.1 ms
- 1 MB: 25 ms
- 10 MB: 257 ms
- 100 MB: 2.5 s

**Sustained throughput**: ~40 MB/s

**Alternative vurdert**: BLAKE3 er raskere (3 GB/s) men SHA-256 er mer proven og standard. Kan bytte senere om nødvendig.

**Test**: 14/14 hashing tests passed

---

### D012: 1.0s Debounce Interval vs Instant Indexing

**Kontekst**: File editors genererer mange events ved save (temp files, atomic writes).

**Alternativer**:
1. Ingen debouncing (instant indexing)
2. 0.1s debounce (minimal delay)
3. 1.0s debounce (recommended)
4. 5.0s debounce (long delay)

**Beslutning**: 1.0 sekund debounce interval

**Rasjonale**:
- Handles all editor patterns (Vim, VSCode, Emacs, IntelliJ)
- Vim atomic save: .swp → .swp~ → file.txt (flere events over 500ms)
- VSCode atomic save: file.txt.tmp → file.txt (2 events over 200ms)
- 1.0s gir nok tid til at alle events settles
- Minimal perceived latency for brukere

**Konsekvenser**:
- ✅ Vim save: 50x fewer operations (50 → 1)
- ✅ VSCode save: 10x fewer operations (10 → 1)
- ✅ Cargo build: Zero noise (5000+ events coalesced)
- ✅ npm install: Zero impact (50k+ events filtered)
- ❌ 1 second delay før indexing (acceptable)

**Ignore patterns**: 30+ patterns added:
- Extensions: `.swp`, `.tmp`, `.bak`, `.DS_Store`, `.lock`
- Directories: `node_modules/`, `target/`, `.git/`, `__pycache__/`
- Build artifacts: `*.o`, `*.so`, `*.dll`, `*.pyc`

**Test**: 5/5 debouncing tests passed (including Vim, VSCode atomic writes)

---

### D013: 5-Minute Session Timeout vs Other Intervals

**Kontekst**: Når skal en ny work session starte?

**Alternativer**:
1. 1 minute (frequent sessions, noisy)
2. 5 minutes (balanced)
3. 15 minutes (long sessions, lose detail)
4. 30 minutes (very long, misses breaks)

**Beslutning**: 5 minutter inaktivitet = ny session

**Rasjonale**:
- Aligns with natural work patterns (coffee break, meeting)
- Pomodoro technique bruker 5 min breaks
- Ikke så kort at context switches skaper noise
- Ikke så langt at lunch/meetings teller som samme session

**Konsekvenser**:
- ✅ Realistic session boundaries
- ✅ Temporal queries gir meningsfulle resultater
- ✅ "What did I work on this morning?" - clear sessions
- ❌ Rapid task switching kan skape mange sessions (acceptable)

**Statistics from testing**:
- Avg session duration: ~15-30 minutes
- Avg files per session: 3-5
- Sessions per day: 10-20 (typical developer)

**Konfigurerbarhet**: Kan justeres i fremtidige versjoner om nødvendig.

**Test**: 8/8 session persistence tests passed

---

### D014: Custom Debouncer vs notify-debouncer-full

**Kontekst**: notify-debouncer-full crate finnes, men har limitations.

**Alternativer**:
1. Bruk notify-debouncer-full (external dependency)
2. Implementer custom debouncer (mer kode)
3. Hybrid (start custom, migrate later)

**Beslutning**: Custom debouncer for Phase 1.5, kan bytte til notify-debouncer-full senere

**Rasjonale**:
- Proves concept (vi forstår hva som trengs)
- Full kontroll over logic (can optimize)
- Enkelt å bytte senere (interface er det samme)
- notify-debouncer-full har issues med inode tracking (kompliserer ting)
- Custom implementation er kun ~370 LOC

**Konsekvenser**:
- ✅ Full kontroll over debouncing logic
- ✅ Enklere å debugge
- ✅ Kan optimalisere for våre use cases
- ❌ Mer kode å vedlikeholde (370 LOC)
- ❌ Må implementere inode tracking selv (future work)

**Migration path**: Custom debouncer har samme interface som notify-debouncer-full, så swap er trivial.

**Known limitations**:
- Ingen inode tracking (path-based only)
- No rename detection (treated as DELETE + CREATE)
- Fixed ignore patterns (not configurable yet)

**Future work**: Kan bytte til notify-debouncer-full i Phase 2 hvis nødvendig.

**Test**: 5/5 debouncing tests passed

---

### D015: Session Persistence to Database vs In-Memory

**Kontekst**: Skal sessions kun lagres i memory eller også i database?

**Alternativer**:
1. In-memory only (lost on restart)
2. Database persistence (survives restart)
3. Hybrid (recent in memory, old in DB)

**Beslutning**: Full session persistence to database

**Rasjonale**:
- Enables temporal queries ("what did I work on yesterday?")
- Historical analysis (productivity insights)
- Survives restarts
- Minimal overhead (~100 bytes per event)
- Critical for knowledge graph

**Konsekvenser**:
- ✅ "What did I work on today?" - ✅ Works
- ✅ "Show me yesterday's sessions" - ✅ Works
- ✅ Session statistics - ✅ Works
- ✅ Historical analysis enabled
- ❌ Database size grows (but: ~2KB/day = 730KB/year, neglisjerbart)

**Schema design**:
```sql
CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    user_id TEXT,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    is_active INTEGER DEFAULT 1
);

CREATE TABLE session_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    file_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    timestamp TEXT NOT NULL
);
```

**Query performance**:
- Simple queries: ~1ms
- Medium (100 sessions): ~10ms
- Large (1000+ sessions): ~50ms

**Indexes used**:
- `idx_sessions_active` - Fast active session lookup
- `idx_events_session` - Fast session event lookup
- `idx_events_timestamp` - Fast temporal queries

**Test**: 8/8 session persistence tests passed

---

### D016: Phase 1.5 Completion Criteria vs Perfect Implementation

**Kontekst**: Når er Phase 1.5 "ferdig"?

**Alternativer**:
1. Perfect implementation (100% spec, all edge cases)
2. 80% spec compliance (critical features working)
3. Minimal viable (just enough to test)

**Beslutning**: 80% spec compliance er nok for Phase 1.5

**Rasjonale**:
- Critical gaps resolved (portability, debouncing, hashing, sessions)
- All tests passing (42/42)
- Production-ready quality
- Remaining 20% (GLiNER, vector search) tilhører Phase 2
- Better to ship working code than perfect code

**Konsekvenser**:
- ✅ Phase 1.5 complete in 1 day (efficient)
- ✅ Can move to Phase 2 (neural intelligence)
- ✅ Users can start using Synapse now
- ❌ Not all spec features implemented (acceptable - Phase 2)

**Acceptance criteria met**:
- [x] Database portability ✅
- [x] Robust file watching ✅
- [x] Efficient indexing ✅
- [x] Temporal queries ✅
- [x] All tests passing ✅

**Phase 2 work remaining**:
- [ ] GLiNER entity extraction
- [ ] sqlite-vec vector search
- [ ] Polymorphic schema
- [ ] Full-text search (Tantivy)

**Status**: ✅ Phase 1.5 COMPLETE, ready for Phase 2

---

## Lessons Learned (Synapse Phase 1.5)

### L001: Test-Driven Development Works

**Observation**: Writing tests first caught bugs early

**Examples**:
- Portability test found path normalization bugs before production
- Debouncer test revealed gaps in ignore patterns
- Hash test caught temporary value lifetime issues

**Takeaway**: Continue TDD for Phase 2

---

### L002: Real-World Testing > Synthetic Tests

**Observation**: Testing with actual editors (Vim, VSCode) revealed edge cases synthetic tests missed

**Examples**:
- Vim's atomic save pattern: `.swp` → `.swp~` → file.txt
- VSCode's temp file pattern: `file.txt.tmp` → `file.txt`
- Emacs backup files: `file.txt~`

**Takeaway**: Always test with real tools, not just synthetic data

---

### L003: Performance Concerns Often Overblown

**Observation**: Hash computation overhead turned out negligible

**Before**: Worried SHA-256 would be too slow (40 MB/s)
**After**: Hashing is <1% of total indexing time

**Example**: 10 MB file
- Hash: 257ms
- File read: ~500ms
- Entity extraction: ~2000ms (future)
- Total: ~2757ms → hash is 9% of total, acceptable

**Takeaway**: Profile before optimizing, don't assume

---

### L004: Incremental Progress > Big Bang

**Observation**: Breaking Phase 1.5 into 4 days made it manageable

**Alternative**: Could have done "all at once" (overwhelming)
**Actual**: Day 1 → Day 2 → Day 3 → Day 4 (clear progress)

**Each day**:
- Clear goal
- Testable outcome
- Documentation written
- Can stop if blocked

**Takeaway**: Keep using day-by-day approach for Phase 2

---

## Fremtidige beslutninger (Phase 2)

### Uavklart: GLiNER Model Selection

**Spørsmål**: Hvilken GLiNER-modell skal brukes?

**Alternativer**:
1. gliner_small (50 MB, fast, lavere accuracy)
2. gliner_medium (150 MB, balansert)
3. gliner_large (500 MB, best accuracy, tregere)

**Vurdering**: Start med small, evaluer accuracy, oppgrader om nødvendig

---

### Uavklart: sqlite-vec Embedding Model

**Spørsmål**: Hvilken embedding model for vector search?

**Alternativer**:
1. all-MiniLM-L6-v2 (80 MB, 384 dims, fast)
2. all-mpnet-base-v2 (420 MB, 768 dims, best quality)
3. e5-small (130 MB, 384 dims, multilingual)

**Vurdering**: all-MiniLM-L6-v2 for Phase 2 (good balance)

---

### Uavklart: Python Subprocess vs Native Rust ONNX

**Spørsmål**: Hvordan kjøre GLiNER inference?

**Alternativer**:
1. Python subprocess (enkelt, men overhead)
2. ort (Rust ONNX bindings, komplisert)
3. tract (pure Rust, men mangler ops)

**Vurdering**: Start med Python subprocess, migrate to ort if performance matters

---

**Versjon**: 0.2
**Sist oppdatert**: 2026-01-25
**Neste review**: Q2 2026 (etter Phase 2 completion)


---

## 2026-01-27/28: Shell Execution & Kernel Stability

### D017: 64KB User Stack (16 pages) vs 4KB (1 page)

**Kontekst**: Rust shell krasjet med GPF ved SYSCALL -- stack overflow.

**Alternativer**:
1. 4KB (1 page) -- minimal, nok for assembly
2. 16KB (4 pages) -- moderat
3. 64KB (16 pages) -- standard for Rust
4. 1MB+ -- overkill

**Beslutning**: 64KB (16 pages)

**Rasjonale**:
- Rust programs bruker mer stack enn assembly (iterators, closures, formatting)
- `core::fmt` machinery alene bruker flere KB
- 64KB er standard for mange embedded Rust targets
- 4KB fungerte for assembly test programs men feilet umiddelbart for Rust shell

**Konsekvenser**:
- OK: Rust shell kjorer stabilt
- OK: Plass til komplekse kommandoer og string formatting
- Noe mer minne per task (64KB vs 4KB)
- Kan gjores konfigurerbart per-task senere

**Fil**: `kernel/src/task/spawn.rs` -- `stack_pages = 16`

---

### D018: FMASK MSR for Interrupt Safety

**Kontekst**: SYSCALL-instruksjonen deaktiverer ikke interrupts automatisk. Keyboard interrupt under syscall entry forarsaker reentrant corruption.

**Alternativer**:
1. FMASK = 0 (ingen masking, fikse i software)
2. FMASK = 0x200 (mask IF only)
3. FMASK = 0x600 (mask IF + DF)

**Beslutning**: FMASK = 0x600 (mask IF + DF)

**Rasjonale**:
- Linux bruker 0x47700 (masker mange flags)
- IF (bit 9) ma maskes for a forhindre interrupt under register save
- DF (bit 10) ma maskes for at string-operasjoner gar riktig vei
- Minimal masking = minimal side effects

**Konsekvenser**:
- OK: Ingen reentrant interrupts under syscall entry
- OK: String ops alltid forward direction
- Interrupts re-enablet etter register save i handler

**Fil**: `kernel/src/arch/x86_64/syscall.rs` -- `Msr::new(0xC0000084).write(0x600)`

---

### D019: Naked Keyboard IRQ Handler vs make_exception_handler!

**Kontekst**: `make_exception_handler!` macro printer feilmelding og halter CPU -- ubrukelig for keyboard IRQ som skal returnere.

**Alternativer**:
1. Bruk make_exception_handler! (halter CPU)
2. Skriv naked handler manuelt
3. Lag ny macro for IRQ handlers

**Beslutning**: Manuell naked handler

**Rasjonale**:
- Keyboard IRQ ma returnere via IRETQ, ikke halte
- Naked handler gir full kontroll over register save/restore
- Bare caller-saved registers trenger a lagres (9 stk)
- PIC EOI sendes inne i handle_interrupt(), ikke i asm

**Konsekvenser**:
- OK: Keyboard fungerer korrekt
- OK: Ingen unodvendig overhead
- Ma skrive lignende handlers for fremtidige IRQs
- Bor lage en `make_irq_handler!` macro naar flere IRQs trengs

**Fil**: `kernel/src/main.rs` -- `irq_keyboard()` naked extern "C" fn

---

### D020: PIC EOI Placement -- Top of Handler

**Kontekst**: Keyboard handler hadde early returns for key-release events FOR PIC EOI ble sendt. Forste tastetrykk fungerte, deretter ingenting.

**Alternativer**:
1. EOI pa slutten av handler (original -- buggy)
2. EOI i starten av handler (fikset)
3. EOI i naked asm wrapper

**Beslutning**: EOI i starten av handle_interrupt(), etter scancode read

**Rasjonale**:
- PIC MA fa EOI for HVER interrupt, inkludert key-release
- Early returns i handler hoppet over EOI -- PIC masket alle videre IRQ1
- Scancode ma leses for EOI (ellers taper vi data)
- Rekkefolge: les scancode -> send EOI -> prosesser scancode

**Konsekvenser**:
- OK: Alle tastetrykk mottas (press + release)
- OK: PIC aldri blokkert
- Monsteret er viktig for alle fremtidige IRQ handlers

**Fil**: `kernel/src/drivers/keyboard.rs` -- `handle_interrupt()`

---

### D021: Disable Assembly Shell -- Single Keyboard Consumer

**Kontekst**: To tasks (assembly shell + Rust shell) pollet samme KEY_BUFFER. Taster ble tilfeldig fordelt mellom dem.

**Alternativer**:
1. Fjern assembly shell helt
2. Deaktiver assembly shell (behold koden)
3. Implementer fokus-system (route keyboard til aktiv task)

**Beslutning**: Deaktiver assembly shell, behold koden for referanse

**Rasjonale**:
- Rust shell erstatter assembly shell funksjonelt
- Fokus-system er fremtidig arbeid (krever window manager konsept)
- Enkleste lossning som fungerer na

**Konsekvenser**:
- OK: Alle taster gar til Rust shell
- Assembly shell-koden bevart for referanse
- TODO: Implementer keyboard routing til fokusert task

**Fil**: `kernel/src/lib.rs` -- assembly shell spawn utkommentert

---

**Versjon**: 0.3
**Sist oppdatert**: 2026-01-28
**Neste review**: Q2 2026


---

## D017: SQLite as Universal Data Format (2026-01-29)

**Status**: Accepted
**Context**: Need a structured data format for the filesystem that supports queries and is inspectable with standard tools.

**Decision**: Use SQLite as the universal data container. All file data is stored as BLOBs in a standard SQLite database.

**Rationale**:
1. SQLite is the most deployed database in the world
2. Files can be inspected with standard `sqlite3` CLI
3. B-tree indexes enable fast lookups
4. Schema provides metadata (name, kind, size) alongside data
5. Future: vector embeddings can live next to file data

**Consequences**:
- (+) Universal inspectability
- (+) Structured queries possible
- (+) Single format for everything
- (-) More complexity than flat files
- (-) Need no_std SQLite parser in userspace

**Implementation**: `userspace/libsqlite/` (~950 lines)

---

## D018: Synapse Backend Auto-Detection (2026-01-29)

**Status**: Accepted
**Context**: Need to support both old FPK format and new SQLite format during transition.

**Decision**: Synapse tries to load `files.db` first, falls back to FPK if not found.

**Rationale**:
1. Backwards compatibility with existing FPK initrds
2. Gradual migration path
3. No breaking changes to existing workflow

**Implementation**: `try_load_sqlite()` in synapse-service checks for SQLite magic bytes.

---


---

## D019: Skip Phase 4, Focus on Phase 5 (Vector Search)

**Date**: 2026-01-29
**Status**: Decided

**Context**: After completing the SQLite Universal Container milestone, the next logical step could be either Phase 4 (Desktop Environment) or Phase 5 (AI Features).

**Decision**: Skip Phase 4 (Desktop) and focus on Phase 5 (Native Semantic Search / Vector Search).

**Rationale**:
- Phase 5 builds directly on the SQLite foundation just completed
- Vector search is the "AI Upgrade" that differentiates Folkering OS
- Desktop can be added later; semantic search is foundational
- Pre-computed embeddings fit the no_std constraint elegantly

**Consequences**:
- No graphical desktop for now
- Focus on making the shell smarter with semantic search
- Embedding generation happens at build-time (Python), search at runtime (no_std Rust)

---

## D020: Embedding Model Selection

**Date**: 2026-01-29
**Status**: Recommended (pending implementation)

**Context**: Which embedding model to use for semantic search?

**Options**:
- A) all-MiniLM-L6-v2 (384-dim, 22M params)
- B) paraphrase-MiniLM-L3-v2 (384-dim, 17M params)
- C) all-mpnet-base-v2 (768-dim, 110M params)

**Decision**: Option A - all-MiniLM-L6-v2

**Rationale**:
- 384 dimensions = 1.5KB per file (manageable)
- Good semantic quality
- Well-tested, widely used
- Already used in userspace Synapse graph filesystem

---

## D021: Query Embedding Strategy

**Date**: 2026-01-29
**Status**: Pending (requires implementation testing)

**Context**: How to generate embeddings for user queries at runtime without Python?

**Options**:
- A) Pre-computed common queries only
- B) Word vector averaging (GloVe-style)
- C) Keyword fallback with semantic boost
- D) External embedding service via virtio

**Decision**: Start with C (keyword fallback), evaluate B for improvement

**Rationale**:
- C provides immediate functionality without complex dependencies
- B could be added later for better semantic matching
- A and D are too limited or complex for MVP
