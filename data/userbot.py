import asyncio
import json
import logging
import os
import ssl
import sys
import subprocess
from pathlib import Path
from typing import Any, Dict, Optional

# Minimal self-bootstrap (no requirements.txt/venv)
def ensure_deps():
    pkgs = [
        ("pyrogram", "pyrogram>=2.0.106"),
        ("aiohttp", "aiohttp>=3.9.0"),
        ("dotenv", "python-dotenv>=1.0.0"),
        ("tgcrypto", "tgcrypto>=1.2.5"),
        ("certifi", "certifi>=2024.2.2"),
    ]
    for mod, spec in pkgs:
        try:
            __import__(mod)
        except Exception:
            subprocess.check_call([sys.executable, "-m", "pip", "install", "--upgrade", spec])

ensure_deps()

from aiohttp import ClientSession, FormData, TCPConnector  # noqa: E402
from dotenv import load_dotenv  # noqa: E402
from pyrogram import Client  # noqa: E402
from pyrogram.handlers import MessageHandler  # noqa: E402
from pyrogram.types import Message  # noqa: E402
import certifi  # noqa: E402

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger("userbot")

# Project paths
PROJECT_DIR = Path(__file__).resolve().parents[1]
DATA_DIR = PROJECT_DIR / "data"
TMP_DIR = DATA_DIR / "tmp"
DATA_DIR.mkdir(exist_ok=True)
TMP_DIR.mkdir(exist_ok=True)

STATE_PATH = DATA_DIR / "state.json"  # {"target_chat_id": int, "subscribed": bool}

def load_json(path: Path, default):
    try:
        with path.open("r", encoding="utf-8") as f:
            return json.load(f)
    except FileNotFoundError:
        return default
    except Exception as e:
        logger.error(f"Failed to load {path}: {e}")
        return default

def file_mtime(path: Path) -> float:
    try:
        return path.stat().st_mtime
    except FileNotFoundError:
        return 0.0

def display_title(chat) -> str:
    title = getattr(chat, "title", None)
    if not title:
        fn = getattr(chat, "first_name", None) or ""
        ln = getattr(chat, "last_name", None) or ""
        title = (fn + (" " + ln if ln else "")).strip()
    return title or getattr(chat, "username", None) or str(chat.id)

class Forwarder:
    def __init__(self, app: Client, bot_token: str):
        self.app = app
        self.bot_token = bot_token
        self._session: Optional[ClientSession] = None
        self._state_mtime: float = 0.0
        self._target_chat_id: Optional[int] = None
        self._subscribed: bool = False
        # SSL context using certifi (fixes broken system CA)
        self._ssl_ctx = ssl.create_default_context(cafile=certifi.where())
        # Optional override: set TELEGRAM_SSL_INSECURE=1 in .env to skip verification (not recommended)
        self._insecure = os.getenv("TELEGRAM_SSL_INSECURE") == "1"

    async def init_http(self):
        connector = TCPConnector(ssl=False) if self._insecure else TCPConnector(ssl=self._ssl_ctx)
        self._session = ClientSession(connector=connector)

    async def close_http(self):
        if self._session:
            await self._session.close()

    def _reload_state_if_changed(self):
        mt = file_mtime(STATE_PATH)
        if mt != self._state_mtime:
            st = load_json(STATE_PATH, {})
            self._target_chat_id = st.get("target_chat_id")
            self._subscribed = bool(st.get("subscribed", False))
            self._state_mtime = mt
            logger.info(f"Reloaded state: target={self._target_chat_id} subscribed={self._subscribed}")

    async def _post_json(self, method: str, payload: Dict[str, Any]):
        assert self._session and self._target_chat_id
        url = f"https://api.telegram.org/bot{self.bot_token}/{method}"
        async with self._session.post(url, json=payload, timeout=45) as resp:
            if resp.status == 429:
                body = await resp.json()
                retry = int(body.get("parameters", {}).get("retry_after", 1))
                await asyncio.sleep(retry)
                return await self._post_json(method, payload)
            if resp.status != 200:
                text = await resp.text()
                logger.warning(f"{method} failed {resp.status}: {text}")

    async def _post_file(self, method: str, file_field: str, file_path: Path, caption: Optional[str]):
        assert self._session and self._target_chat_id
        url = f"https://api.telegram.org/bot{self.bot_token}/{method}"
        data = FormData()
        data.add_field("chat_id", str(self._target_chat_id))
        if caption:
            data.add_field("caption", caption)
        f = file_path.open("rb")
        try:
            data.add_field(file_field, f, filename=file_path.name)
            async with self._session.post(url, data=data, timeout=300) as resp:
                if resp.status == 429:
                    body = await resp.json()
                    retry = int(body.get("parameters", {}).get("retry_after", 1))
                    await asyncio.sleep(retry)
                    return await self._post_file(method, file_field, file_path, caption)
                if resp.status != 200:
                    text = await resp.text()
                    logger.warning(f"{method} failed {resp.status}: {text}")
        finally:
            try:
                f.close()
            except Exception:
                pass

    async def _send_text(self, text: str):
        await self._post_json("sendMessage", {"chat_id": self._target_chat_id, "text": text})

    async def _send_media(self, m: Message, kind: str, caption: Optional[str]):
        # Download to disk, stream upload to Bot API
        path_str = await m.download(file_name=str(TMP_DIR / "media"))
        if not path_str:
            await self._send_text(caption or f"[{kind}]")
            return
        p = Path(path_str)
        try:
            if kind == "photo":
                await self._post_file("sendPhoto", "photo", p, caption)
            elif kind == "video":
                await self._post_file("sendVideo", "video", p, caption)
            elif kind == "document":
                await self._post_file("sendDocument", "document", p, caption)
            elif kind == "audio":
                await self._post_file("sendAudio", "audio", p, caption)
            elif kind == "voice":
                await self._post_file("sendVoice", "voice", p, caption)
            elif getattr(m, "animation", None):
                await self._post_file("sendAnimation", "animation", p, caption)
            else:
                await self._post_file("sendDocument", "document", p, caption or f"[{kind}]")
        finally:
            try:
                p.unlink(missing_ok=True)
            except Exception:
                pass

    async def on_message(self, _app: Client, m: Message):
        # Keep state current
        self._reload_state_if_changed()
        if not self._target_chat_id or not self._subscribed:
            return

        # Ignore your own outgoing messages and bot messages to avoid loops
        if m.outgoing:
            return
        if m.from_user and m.from_user.is_bot:
            return

        # Text or media
        title = display_title(m.chat)
        if m.text:
            await self._send_text(f"[{title}] {m.text}")
            return

        caption = (m.caption or "").strip()
        cap = f"[{title}] {caption}" if caption else f"[{title}]"
        if m.photo:
            await self._send_media(m, "photo", cap)
        elif m.video:
            await self._send_media(m, "video", cap)
        elif m.document:
            await self._send_media(m, "document", cap)
        elif m.audio:
            await self._send_media(m, "audio", cap)
        elif m.voice:
            await self._send_media(m, "voice", cap)
        elif getattr(m, "animation", None):
            await self._send_media(m, "animation", cap)
        else:
            await self._send_text(cap)

async def main():
    # Load .env from the project root
    load_dotenv(PROJECT_DIR / ".env")
    api_id = os.getenv("API_ID")
    api_hash = os.getenv("API_HASH")
    bot_token = os.getenv("TELEGRAM_BOT_TOKEN")

    if not all([api_id, api_hash, bot_token]):
        raise RuntimeError("Missing API_ID, API_HASH or TELEGRAM_BOT_TOKEN in .env")

    app = Client("user", api_id=int(api_id), api_hash=api_hash)
    await app.start()  # First run prompts for login
    logger.info("Userbot (Pyrogram) signed in.")

    fwd = Forwarder(app, bot_token)
    await fwd.init_http()
    fwd._reload_state_if_changed()

    app.add_handler(MessageHandler(fwd.on_message))

    try:
        logger.info("Userbot running. Use the bot button to Subscribe/Unsubscribe.")
        await asyncio.Event().wait()
    finally:
        await fwd.close_http()
        await app.stop()

if __name__ == "__main__":
    asyncio.run(main())