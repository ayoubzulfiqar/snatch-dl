# Security

## Found a security hole?

**Do not open a public issue.** A public issue tells attackers before we can
fix it.

Report it one of these two ways:

1. **Best:** use GitHub's private form.
   [Report a vulnerability](https://github.com/ayoubzulfiqar/snatch-dl/security/advisories/new)
2. **Or:** email **ayoubzulfiqar3@gmail.com** with `SECURITY` in the subject.

## What to tell us

The more you give us, the faster we fix it.

- What the problem is, in a sentence.
- The steps to make it happen.
- What version of Snatch you ran. Find it with `rpm -q snatch-dl`,
  `dpkg -s snatch-dl`, or `pacman -Q snatch-dl`.
- What Linux you use.
- What an attacker could do with it.

If you have a proof of concept, send it. Please do not run it against anyone
else's computer.

## What we will do

| When | What |
|---|---|
| Within 3 days | We reply and say we got it. |
| Within 7 days | We tell you if we agree it is a bug, and how bad we think it is. |
| Within 30 days | We aim to have a fix released. |

If a fix will take longer, we will tell you why.

We will credit you when we announce the fix, unless you would rather we did
not. Just say.

## Versions we fix

Only the newest release. Snatch is young and moves fast. Please update before
you report.

## Where the risk is

These are the parts worth looking at. They handle input from outside.

| Part | What comes in |
|---|---|
| `snatch-nmh` | Messages from the browser add-on |
| `ipc.rs` | Anything written to the Unix socket |
| `sniff.rs` | HTML from any web page |
| `curl.rs` | A pasted `curl` command |
| `batch.rs` | A typed URL pattern |
| `archive.rs` | Archive files, which may be built to attack you |
| `mirror.rs` | Pages found while crawling a site |

## What we already do

- Snatch never runs `sudo`.
- URLs are checked against a list of allowed schemes. `file:` and `data:` are
  refused.
- Filenames from the web are cut down to the last part, so `../../etc/passwd`
  cannot escape.
- Control characters are stripped out of headers.
- Passwords are never written to disk, and never passed on a command line
  where `ps` would show them.
- The socket at `~/.local/share/snatch-dl/snatch.sock` is mode `0600`. Only you
  can use it.
- Snatch has **no network listener**. Nothing can reach it from another
  machine. This was a deliberate choice.
- Downloaded tools are checked against their published SHA-256 sums before
  they run.

## Not a security bug

- Snatch can download files that are illegal where you live. That is your
  choice, not a bug.
- Turning off TLS checks in Settings is unsafe. It is meant to be. It says so.
- The browser add-on needs wide permissions to catch downloads. That is how
  browser add-ons work.
