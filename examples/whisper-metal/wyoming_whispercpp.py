"""A Wyoming speech-to-text server in front of whisper.cpp's whisper-server."""
import argparse, asyncio, io, json, logging, time, urllib.request, wave
from wyoming.asr import Transcribe, Transcript
from wyoming.audio import AudioChunk, AudioStart, AudioStop
from wyoming.event import Event
from wyoming.info import AsrModel, AsrProgram, Attribution, Describe, Info
from wyoming.server import AsyncEventHandler, AsyncServer

log = logging.getLogger("wyoming-whispercpp")

class Handler(AsyncEventHandler):
    def __init__(self, info, server, language, *args, **kw):
        super().__init__(*args, **kw); self.info = info; self.server = server; self.language = language
        self.pcm = b""; self.rate = 16000; self.width = 2; self.channels = 1
    async def handle_event(self, event: Event) -> bool:
        if Describe.is_type(event.type):
            await self.write_event(self.info.event()); return True
        if Transcribe.is_type(event.type):
            t = Transcribe.from_event(event); self.language = t.language or self.language; return True
        if AudioStart.is_type(event.type):
            s = AudioStart.from_event(event); self.rate, self.width, self.channels = s.rate, s.width, s.channels; self.pcm = b""; return True
        if AudioChunk.is_type(event.type):
            self.pcm += AudioChunk.from_event(event).audio; return True
        if AudioStop.is_type(event.type):
            text = await asyncio.get_running_loop().run_in_executor(None, self.transcribe); await self.write_event(Transcript(text=text).event()); return False
        return True
    def transcribe(self) -> str:
        buf = io.BytesIO()
        with wave.open(buf, "wb") as w:
            w.setnchannels(self.channels); w.setsampwidth(self.width); w.setframerate(self.rate); w.writeframes(self.pcm)
        boundary = "wyomingwhispercpp"; body = io.BytesIO()
        for name, value in (("response_format", "json"), ("language", self.language), ("temperature", "0.0")):
            body.write(f"--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n".encode())
        body.write(f"--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\nContent-Type: audio/wav\r\n\r\n".encode()); body.write(buf.getvalue()); body.write(f"\r\n--{boundary}--\r\n".encode())
        req = urllib.request.Request(self.server + "/inference", data=body.getvalue(), headers={"Content-Type": f"multipart/form-data; boundary={boundary}"})
        t = time.perf_counter()
        with urllib.request.urlopen(req, timeout=300) as r: text = json.loads(r.read()).get("text", "").strip()
        log.info("%.2fs of audio in %.2fs: %r", len(self.pcm) / (self.rate * self.width * self.channels), time.perf_counter() - t, text)
        return text

async def main():
    p = argparse.ArgumentParser(); p.add_argument("--uri", default="tcp://0.0.0.0:10300"); p.add_argument("--server", default="http://127.0.0.1:8080"); p.add_argument("--language", default="en"); a = p.parse_args()
    logging.basicConfig(level=logging.INFO)
    info = Info(asr=[AsrProgram(name="whisper.cpp", description="whisper.cpp on lighter.sh/metal", attribution=Attribution(name="ggml-org", url="https://github.com/ggml-org/whisper.cpp"), installed=True, version="1", models=[AsrModel(name="ggml", description="ggml whisper model", attribution=Attribution(name="OpenAI", url="https://github.com/openai/whisper"), installed=True, languages=[a.language], version="1")])])
    server = AsyncServer.from_uri(a.uri); log.info("ready on %s", a.uri)
    await server.run(lambda *args, **kw: Handler(info, a.server, a.language, *args, **kw))
asyncio.run(main())
