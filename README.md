# teleport-box

`teleport-box` runs a local program inside a fake remote Linux environment built from `sshfs`, `ssh`, and `bwrap`.

## What it is

It is a local tool that makes a process running on your machine see a remote filesystem and execute commands on a remote Linux host, without installing any helper on the server side.

## What it is for

The main use case is offline devices and similar environments where you need fast remote debugging with a local Codex instance.

Instead of deploying a separate agent on the server:

- you use plain SSH
- you keep Codex local
- you work against remote files and remote tools

## How it works

`teleport-box` combines three pieces:

- `sshfs` mounts the remote filesystem
- `ssh` executes remote commands
- `bwrap` isolates the local process inside a sandbox that looks like one coherent remote environment

The core rule is simple:

- remote tools are truth
- local host tools are denied by default

If a binary does not exist on the remote host, the correct behavior is failure, not a silent local fallback.

## Command syntax

```bash
teleport-box <doctor|shell|exec|codex> [user@]host[:port] [options] [-- command]
```

Common examples:

```bash
teleport-box doctor root@host --identity-file ~/.ssh/id_ed25519
teleport-box shell root@host --identity-file ~/.ssh/id_ed25519
teleport-box exec root@host --identity-file ~/.ssh/id_ed25519 -- sh -c 'uname -a'
teleport-box codex root@host --identity-file ~/.ssh/id_ed25519 -- exec -C /root "Run uname -a"
```

## Dependencies

The project relies on:

- `sshfs` for mounting the remote filesystem
- `ssh` for remote execution
- `bwrap` for the local sandbox

It requires Linux on the local host side and a Linux-like remote host with SSH, SFTP, and `sh`.
