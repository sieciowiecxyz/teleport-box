# teleport-box

`teleport-box` uruchamia lokalny program w fake remote Linux environment zbudowanym z `sshfs`, `ssh` i `bwrap`.

## Co to jest

To lokalne narzędzie, które sprawia, że proces uruchomiony na twoim hoście widzi zdalny filesystem i wykonuje komendy na zdalnym Linuksie, bez instalowania helpera po stronie serwera.

## Po co to jest

Główny use case to offline devices i inne środowiska, gdzie trzeba szybko debugować zdalny system przy pomocy lokalnego Codexa.

Zamiast stawiać osobnego agenta na serwerze:

- używasz zwykłego SSH
- trzymasz lokalnego Codexa
- pracujesz na zdalnych plikach i zdalnych toolach

## Jak to działa

`teleport-box` skleja trzy elementy:

- `sshfs` montuje zdalny filesystem
- `ssh` wykonuje zdalne komendy
- `bwrap` zamyka lokalny proces w sandboxie, który wygląda jak spójne zdalne środowisko

Zasada projektu jest prosta:

- remote tools are truth
- local host tools are denied by default

Jeżeli binarki nie ma na zdalnym hoście, to ma być błąd, a nie lokalny fallback.

## Składnia komendy

```bash
teleport-box <doctor|shell|exec|codex> [user@]host[:port] [opcje] [-- komenda]
```

Najczęstsze przykłady:

```bash
teleport-box doctor root@host --identity-file ~/.ssh/id_ed25519
teleport-box shell root@host --identity-file ~/.ssh/id_ed25519
teleport-box exec root@host --identity-file ~/.ssh/id_ed25519 -- sh -c 'uname -a'
teleport-box codex root@host --identity-file ~/.ssh/id_ed25519 -- exec -C /root "Run uname -a"
```

## Z czego korzysta

Projekt korzysta z:

- `sshfs` do montowania zdalnego filesystemu
- `ssh` do zdalnego exec
- `bwrap` do lokalnego sandboxa

Wymagany jest Linux po stronie hosta lokalnego oraz Linux-like host zdalny z SSH, SFTP i `sh`.
