# lighter 0.13.10

`lighter doctor` now tells a stream that could not be joined in the guest's kernel from one that should not be. 0.13.9's `streams` row counted every stream whose TCP socket had already closed one way as a refused join, and on @skwiz0d's Mac that was 103 of 263 outbound streams (#107): enough to look like a fault, when copying is the right way to carry them. Thanks to @skwiz0d for running 0.13.9 straight away and posting the row that showed it.

## What went wrong

**Streams already closed one way were counted as refused joins** (#107). The guest's agent joins a stream's two sockets in its kernel once there is a byte to carry, and the kernel joins only an established TCP socket. A client that writes its request and shuts its side straight away, as `nc` and many HTTP/1.0 tools do, or a server that answers and closes, leaves the socket in CLOSE_WAIT by then. The agent asked the kernel anyway, the kernel refused (`Not supported (os error 95)`), and the stream was copied, as it should be, but `doctor` reported each one as a refusal, and nothing said which socket the kernel had refused.

**On Homebrew 7, `brew install lighter` and `brew upgrade lighter` refused to load the formula.** Homebrew 7 loads formulae only from taps the user has trusted, and `brew tap` does not trust one. The error was `Refusing to load formula fieldwork-ai/tap/lighter from untrusted tap fieldwork-ai/tap`.

## What changed

- **A stream whose TCP socket has already closed one way is copied without asking the kernel**, and counted as closed rather than refused.
- **`lighter doctor`'s `streams` row** says how many of the copied streams had already closed one way, and a refused join says which end the kernel would not take, the TCP socket (with its state) or the stream to the Mac. A guest whose only copies are such streams is not reported as one without kernel joins.
- **The install and update instructions** include `brew trust fieldwork-ai/tap`, once, on Homebrew 7.

## Upgrading

On Homebrew 7, trust the tap once first:

```
brew trust fieldwork-ai/tap
brew upgrade lighter
lighter restart
```

If `brew services` starts lighter, `brew services restart lighter` instead of `lighter restart`. Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script.

## Also

- The guest is 0.13.9's: Linux **6.18.52** with the same configuration and patches, and the same Alpine packages, with only lighter's agent rebuilt. The data epoch remains **1**.
- Gate m3-streams checks that half-closing clients are copied as closed, with no join refused.
