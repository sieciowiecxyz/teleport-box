# teleport-box

`teleport-box` uruchamia lokalną binarkę wewnątrz sandboxa, którego "świat" jest w dużej mierze zdalny:

- pliki pochodzą z remote hosta przez `sshfs`
- komendy są wykonywane na remote hoście przez `ssh`
- lokalny proces, który siedzi w środku, widzi to jako jeden spójny environment

Obecny główny use case to uruchamianie **stockowego lokalnego `codex`** tak, aby pracował na zdalnym Linuksie bez instalowania helpera, daemona ani agenta na serwerze. Po stronie serwera zakładamy wyłącznie `sshd` i standardowe narzędzia systemowe.

## Status

To jest działający PoC.

Potwierdzone:

- `teleport-box exec` zwraca zdalny `uname -a`
- `teleport-box exec` może czytać i pisać pliki na zdalnym hoście
- `teleport-box codex` potrafi wykonać zdalne komendy przez stockowego Codexa
- Codex utworzył i uruchomił plik Pythona na zdalnym Hetznerze
- instalacja pakietu przez `apt-get` przeszła przez teleport-box i wykonała się zdalnie

To nadal nie jest "prawdziwy remote kernel" ani VM. To jest celowo agresywna iluzja zbudowana na `bwrap + sshfs + ssh`.

## Jak to działa

`teleport-box` składa się z czterech warstw:

1. `sshfs` montuje zdalny `/` do lokalnego katalogu runtime.
2. `bwrap` uruchamia lokalny sandbox z kontrolowanym widokiem świata.
3. Wybrane katalogi zdalne, np. `/root`, `/home`, `/usr/local`, są bind-mountowane do sandboxa.
4. Executables w sandboxie są przekierowywane do wrappera, który odpala odpowiednią komendę na zdalnym hoście przez `ssh`.

Ważne rozróżnienie:

- filesystem pathy podpięte przez `sshfs` obsługują bezpośrednie `std::fs`/`tokio::fs`
- execution pathy są teleportowane przez wrappery i launcher interception

To właśnie rozdzielenie file path i exec path sprawia, że Codex może "myśleć", że pracuje na zdalnym systemie, mimo że jego proces jest lokalny.

## Wymagania

Host lokalny:

- Linux
- `ssh`
- `sshfs`
- `bubblewrap` (`bwrap`)
- `fusermount3` albo `fusermount`
- lokalny `codex`
- lokalny `node`

Host zdalny:

- Linux z działającym `sshd`
- standardowe narzędzia systemowe potrzebne do pracy
- brak requirementu na `codex`, ACP, helpery, daemony

## Użycie

Smoke test:

```bash
cargo run --manifest-path /home/sieciowiec/dev/acp-poc/teleport-box/Cargo.toml -- \
  exec \
  --host 178.104.127.252 \
  --user root \
  --identity-file /home/sieciowiec/.ssh/acp-poc-hetzner_ed25519 \
  --remote-cwd /root \
  -- bash -lc 'uname -a'
```

Uruchomienie Codexa:

```bash
/home/sieciowiec/dev/acp-poc/teleport-box/target/debug/teleport-box \
  codex \
  --host 178.104.127.252 \
  --user root \
  --identity-file /home/sieciowiec/.ssh/acp-poc-hetzner_ed25519 \
  -- \
  --dangerously-bypass-approvals-and-sandbox \
  exec \
  --skip-git-repo-check \
  -C /root \
  "Run 'uname -a' and print only the command output."
```

Dlaczego `--dangerously-bypass-approvals-and-sandbox`?

Bo sandboxem jest tutaj `teleport-box`. Wewnętrzny sandbox Codexa blokował wcześniej sockety i psuł SSH.

## Co dziś jest twarde, a co nie

### Twarde

- zdalny filesystem pod zamontowanymi katalogami
- zdalny shell dla wrapperowanych command paths
- zdalne komendy wykonywane przez Codexa w typowych flow typu `bash -lc ...`
- brak helpera po stronie serwera

### Nietwarde

- to nie jest prawdziwe przeniesienie kernela
- `/proc`, `/sys`, `/dev`, sygnały, PID-y i job control są nadal lokalne
- absolutne ścieżki do binarek nadal są szczególnym przypadkiem
- część local control-plane Codexa nadal istnieje, np. `~/.codex`, rollout files, shell snapshots

## Aktualna strategia wrapperów

Projekt nie polega już wyłącznie na ręcznym dopisywaniu każdej komendy.

Obecnie są dwie warstwy:

1. Auto-discovery command names:
   - lokalne executables z typowych bin dirów
   - zdalne executables z remote `PATH`
   - dla nich budowana jest symlink farm w `/codex/wrappers`

2. Curated absolute launcher interception:
   - mała lista krytycznych launcherów/interpreterów, np. `bash`, `sh`, `zsh`, `env`, `python`, `apt-get`
   - to jest obecny kompromis, bo próba owrapowania absolutnie wszystkiego przez pojedyncze bind-mounty rozwaliła się na `ARG_MAX`

Wniosek:

- komendy wywoływane **po nazwie** działają już dużo bardziej ogólnie
- komendy wywoływane **po absolutnej ścieżce** nadal wymagają bardziej agresywnej strategii

## Hardening roadmap

Docelowy kierunek nie powinien opierać się na coraz dłuższej liście `ABSOLUTE_WRAP_COMMANDS`.

Najbardziej sensowna roadmapa:

1. Rozdzielić:
   - `remote command discovery`
   - `absolute exec interception`
   - `local control-plane binaries`

2. Przejść z curated file binds na bardziej ogólny exec layer, np.:
   - synthetic executable directories mountowane jako `/usr/bin`, `/bin`, `/usr/local/bin`
   - albo dedykowany launcher shim dla wybranych katalogów, nie pojedynczych plików

3. Trzymać hostowe narzędzia control-plane poza `PATH` sandboxa:
   - np. jawnie używać `/usr/bin/ssh` tylko z warstwy teleport-box
   - nie pozwolić, aby wrappery same połknęły `ssh` i `sshfs`

4. Utrzymać wsparcie dla custom binary na serwerze:
   - jeśli jest w remote `PATH`, ma działać bez ręcznego dopisywania
   - jeśli jest wywoływany po absolutnej ścieżce, trzeba dodać bardziej ogólną strategię niż obecna allowlista

5. Zachować minimalizm:
   - zero helpera na serwerze
   - zero forka Codexa, dopóki exec/file illusion daje radę

## Znane problemy

- shell snapshoty Codexa potrafią logować warningi
- absolute-path interception nadal jest kompromisem
- startup jest zależny od `sshfs`, więc problemy FUSE odbijają się na całym projekcie
- to jest Linux-first rozwiązanie

## Portability

### Linux host -> Linux remote

To jest wspierany kierunek i aktualny target projektu.

### macOS host

Teoretycznie częściowo możliwe, ale obecny kod jest linux-centric:

- `bwrap` odpada
- `sshfs` bywa problematyczne lub wymaga innego stacku FUSE
- trzeba by zrobić inny sandbox/runtime layer dla hosta

### Windows host

Obecna architektura praktycznie nie przenosi się 1:1:

- brak `bwrap`
- inne semantics paths i exec
- trzeba by pisać osobny host backend

### macOS / Windows remote

Remote filesystem może być widoczny, ale execution semantics przestają być linuksowe. Ten projekt zakłada, że zdalny host ma:

- shell zgodny z `sh`/`bash`
- linuksowe layouty katalogów
- klasyczne userland tools

Czyli: obecna wersja to **Linux host -> Linux remote**.

## Cel projektu

To nie jest ogólny remote desktop ani agent framework.

To jest próba zrobienia czegoś bardzo prostego:

- jedna lokalna instalacja Codexa
- zero helpera na serwerze
- czyste SSH po stronie zdalnej
- maksymalnie przekonująca iluzja, że Codex pracuje "tam", a nie "tu"
