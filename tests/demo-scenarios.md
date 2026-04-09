# Folkering OS — 20 Demo Scenarios

## Kategori 1: The Agentic Loop (ReAct & Verktøy)

### 1. Systeminspektøren
Start QEMU i TCG-modus. Injiser `agent Bruk system_info og list_tasks for å gi meg en komplett statusrapport`. Verifiser at agenten kaller begge verktøyene og gir et tekst-svar.

### 2. Fil-detektiven
Boot OS-et og kjør `agent Bruk list_files for å se hva som finnes. Finn en tekstfil, bruk read_file for å lese den, og fortell meg innholdet.` Verifiser at ReAct-løkken håndterer to verktøy-kall etter hverandre.

### 3. Selv-diagnostikk via Shell
Injiser `agent Bruk run_command for å kjøre en echo-kommando som sier 'AI lever'. Les resultatet.` Sjekk loggene for shell-interaksjon.

### 4. Logisk Betingelse
Send `agent Sjekk system_info. Hvis oppetiden er under 60 sekunder, svar 'Nylig bootet'. Ellers svar 'Gammelt system'.` Verifiser at agenten fatter en beslutning basert på verktøy-data.

## Kategori 2: WASM Generering

### 5. Bouncing Ball
Injiser `agent generate_wasm en sprettende rød ball som beveger seg og spretter mot veggene`. Bekreft `[MCP] Interactive WASM app launched` i loggen.

### 6. Stress-test Descriptor Chaining
Send `agent generate_wasm en app som tegner 100 tilfeldige sirkler i forskjellige farger som endrer posisjon hver frame`. Sjekk TIMING-loggen.

### 7. Auto-Unsafe Filter
Be om `agent generate_wasm en app med et blått rektangel, men uten unsafe-blokker`. Verifiser `fix_unsafe_calls` i proxy-loggen.

### 8. Interaktiv Klikk-Teller
Send `agent generate_wasm en interaktiv app som viser 0 i midten og øker med 1 for hvert museklikk`. Bekreft `PersistentWasmApp` i loggene.

### 9. Uendelig Løkke-filter
Send `agent generate_wasm en app med en bevisst loop {} og spin_loop()`. Verifiser `fix_infinite_loop` i proxy-loggen.

## Kategori 3: Draug Daemon

### 10. Vekke Draug
Start QEMU. La systemet stå urørt i 65 sekunder. Bekreft `[Draug] Analysis started` i serial.log.

### 11. Draug Minne-Alarm
Alloker minne over 85%. Vent på Draug-tick. Bekreft at et `[Draug]`-varsel dukker opp.

### 12. Draug Observasjons-Logg
Boot OS, start/stopp oppgaver. Sjekk MCP-trafikk for at observasjoner inkluderes i Draug-prompten.

## Kategori 4: MCP Protocol & Context Manager

### 13. TimeSync over COBS
Boot OS og bekreft at `[MCP] TimeSync: UTC+` dukker opp i serial.log (COBS/Postcard, ikke legacy JSON).

### 14. Context AutoCompact
Send 15 lange spørsmål via gemini-kommandoen. Bekreft `[CTX] AUTO COMPACT` i proxy-loggen.

### 15. CRC-16 Korrupsjonstest
Hack mcp_bridge.py til å flippe én bit i CRC. Verifiser at OS-et dropper pakken uten krasj.

## Kategori 5: Kompleks Orkestrering

### 16. AI-Skrevet AI-Verktøy
Send `agent Bruk list_files. Generer deretter en WASM-app som tegner navnet på den første filen i grønn tekst`.

### 17. Dynamisk Klokke
Send `agent Sjekk system_info for oppetid. Generate_wasm en app som viser oppetiden på skjermen`.

### 18. IQE Latency under Load
Kjør iqe-automated-tests.py mens en WASM-kompilering pågår i bakgrunnen. Verifiser at split-tidene holder seg under 1ms.

### 19. Escaping The App
Generer en skjermsparer via WASM. Send ESC via QMP. Bekreft at minnet frigjøres og skrivebordet gjentegnes.

### 20. Draug Loop Test
La OS-et stå idle til Draug trigger analyse. Fang opp MCP-teksten og vis den i et nytt vindu via QMP.
