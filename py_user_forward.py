import os
import re
import json
import asyncio
import tempfile
import logging
from pathlib import Path
from dotenv import load_dotenv
from pyrogram import Client, filters
from pyrogram.types import MessageEntity, Message

load_dotenv()

API_ID = int(os.getenv("API_ID", "0"))
API_HASH = os.getenv("API_HASH")
SESSION = os.getenv("PY_SESSION", "user")
SOURCE = os.getenv("FORWARD_SOURCE")     # chat id or username to listen to
TARGET = os.getenv("FORWARD_TARGET")     # where to post copies
OUTPUT = os.getenv("PARSED_OUTPUT", "data/parsed_messages.json")
RATE_DELAY = float(os.getenv("FORWARD_RATE_DELAY", "0.2"))
SKIP_VIDEOS = os.getenv("SKIP_VIDEOS", "1") == "1"

if not API_ID or not API_HASH or not SOURCE or not TARGET:
    raise SystemExit("Set API_ID, API_HASH, FORWARD_SOURCE and FORWARD_TARGET in .env")

# normalize ids (ensure negative channel ids become int)
def _maybe_int(s):
    if s is None:
        return s
    if isinstance(s, int):
        return s
    s = str(s).strip()
    return int(s) if s.lstrip("-").isdigit() else s

SOURCE = _maybe_int(SOURCE)
TARGET = _maybe_int(TARGET)

OUT_PATH = Path(OUTPUT)
OUT_PATH.parent.mkdir(parents=True, exist_ok=True)
TMP_DIR = Path("data/tmp")
TMP_DIR.mkdir(parents=True, exist_ok=True)

URL_RE = re.compile(
    r"""(?xi)
    (?:(https?://)[^\s<>"'“”«»]+)|
    (t\.me/[A-Za-z0-9_/-]+)|
    (telegram\.me/[A-Za-z0-9_/-]+)
    """
)

def extract_urls_from_text(text: str):
    if not text:
        return []
    return [m[0] or m[1] or m[2] for m in URL_RE.findall(text)]

def extract_entity_urls(message: Message):
    out = []
    if not message.entities:
        return out
    raw = message.text or message.caption or ""
    for ent in message.entities:
        # text_url has .url attribute, url needs slicing
        if getattr(ent, "type", None) == "text_url":
            if hasattr(ent, "url") and ent.url:
                out.append(ent.url)
        elif getattr(ent, "type", None) == "url":
            try:
                out.append(raw[ent.offset: ent.offset + ent.length])
            except Exception:
                continue
    return out

def media_types_of(message: Message):
    types = []
    if message.photo:
        types.append("photo")
    if message.video:
        types.append("video")
    if message.sticker:
        types.append("sticker")
    if message.animation:
        types.append("animation")
    if message.voice:
        types.append("voice")
    if message.audio:
        types.append("audio")
    if message.document:
        # inspect mime
        mt = getattr(message.document, "mime_type", "") or ""
        if mt.startswith("video"):
            types.append("video")
        elif mt.startswith("image"):
            types.append("image")
        else:
            types.append("document")
    return types

async def _send_media_copy(client: Client, msg: Message, target, caption_text):
    # download to temp file and send with appropriate API call
    fp = None
    try:
        fp = await msg.download(file=TMP_DIR)  # returns path or None
        if not fp:
            # fallback: send caption only
            await client.send_message(target, caption_text or "[медиа]")
            return

        # choose send method by detected media
        if msg.photo:
            await client.send_photo(target, photo=fp, caption=caption_text or None)
        elif msg.video:
            await client.send_video(target, video=fp, caption=caption_text or None)
        elif msg.sticker:
            await client.send_sticker(target, sticker=fp)
        elif msg.animation:
            await client.send_animation(target, animation=fp, caption=caption_text or None)
        elif msg.voice:
            await client.send_voice(target, voice=fp, caption=caption_text or None)
        elif msg.audio:
            await client.send_audio(target, audio=fp, caption=caption_text or None)
        elif msg.document:
            await client.send_document(target, document=fp, caption=caption_text or None)
        else:
            # generic fallback
            await client.send_document(target, document=fp, caption=caption_text or None)
    finally:
        try:
            if fp:
                Path(fp).unlink(missing_ok=True)
        except Exception:
            pass

app = Client(SESSION, api_id=API_ID, api_hash=API_HASH)

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger("userbot")

@app.on_message(filters.chat(SOURCE) & ~filters.me)
async def handler(client: Client, message: Message):
    try:
        logger.info("msg received id=%s chat=%s from=%s", getattr(message, "message_id", None), getattr(message.chat, "id", None), getattr(message.from_user, "id", None))
        # skip outgoing or bot messages (Pyrogram uses `outgoing` attribute)
        if getattr(message, "outgoing", False) or (getattr(message, "from_user", None) and getattr(message.from_user, "is_bot", False)):
            logger.debug("skipping outgoing/bot message")
            return

        media_types = media_types_of(message)
        if SKIP_VIDEOS and "video" in media_types:
            logger.debug("skipping video message id=%s", getattr(message, "message_id", None))
            return

        raw_text = message.text or message.caption or ""
        urls = extract_urls_from_text(raw_text)
        urls += extract_entity_urls(message)
        # dedupe & normalize
        urls = [u.strip() for u in urls if u and isinstance(u, str)]
        seen = []
        for u in urls:
            if u not in seen:
                seen.append(u)
        urls = seen

        chat_title = getattr(message.chat, "title", None) or getattr(message.chat, "username", None) or str(getattr(message.chat, "id", ""))
        # prepare caption: include original text and explicit links
        caption_lines = []
        if raw_text:
            caption_lines.append(f"[{chat_title}] {raw_text}")
        else:
            caption_lines.append(f"[{chat_title}]")
        if urls:
            caption_lines.append("\nСсылки:")
            caption_lines.extend(urls)
        caption_text = "\n".join(caption_lines)

        parsed = {
            "message_id": message.message_id,
            "chat_id": getattr(message.chat, "id", None),
            "chat_title": chat_title,
            "date": message.date.isoformat() if message.date else None,
            "from_id": getattr(message.from_user, "id", None) if message.from_user else None,
            "from_username": getattr(message.from_user, "username", None) if message.from_user else None,
            "text": raw_text,
            "urls": urls,
            "media_types": media_types,
        }
        # append to JSONL
        with OUT_PATH.open("a", encoding="utf-8") as f:
            f.write(json.dumps(parsed, ensure_ascii=False) + "\n")

        # send/copy to target: preserve media where possible, send text otherwise
        if media_types:
            try:
                logger.info("sending media id=%s to target=%s", parsed["message_id"], TARGET)
                await _send_media_copy(client, message, TARGET, caption_text)
                logger.info("media sent id=%s", parsed["message_id"])
            except Exception:
                logger.exception("failed to send media id=%s to target=%s", parsed["message_id"], TARGET)
        else:
            try:
                logger.info("sending text id=%s to target=%s", parsed["message_id"], TARGET)
                await client.send_message(TARGET, caption_text or "[пустое сообщение]")
                logger.info("text sent id=%s", parsed["message_id"])
            except Exception:
                logger.exception("failed to send text id=%s to target=%s", parsed["message_id"], TARGET)

        await asyncio.sleep(RATE_DELAY)
    except Exception as e:
        logger.exception("handler error")

if __name__ == "__main__":
    async def _main():
        await app.start()
        logger.info("client started; SOURCE=%r TARGET=%r SKIP_VIDEOS=%s", SOURCE, TARGET, SKIP_VIDEOS)

        # list a few dialogs so you can verify the session sees the source chat
        try:
            dialogs = []
            async for d in app.get_dialogs(limit=50):
                dialogs.append((getattr(d.chat, "id", None), getattr(d.chat, "title", None), getattr(d.chat, "username", None)))
            logger.info("recent dialogs (id,title,username): %s", dialogs)
            logger.info("Does session contain SOURCE? %s", any(d[0] == SOURCE for d in dialogs))
        except Exception:
            logger.exception("failed to list dialogs")

        # DEBUG: log every incoming update (enable only while debugging)
        @app.on_message(filters.all)
        async def _debug_all(_, message):
            logger.debug("DBG incoming msg id=%s chat=%s from=%s media=%s text=%s",
                         getattr(message, "message_id", None),
                         getattr(message.chat, "id", None),
                         getattr(message.from_user, "id", None) if getattr(message, "from_user", None) else None,
                         bool(getattr(message, "media", None) or getattr(message, "photo", None) or getattr(message, "video", None)),
                         (message.text or message.caption or "")[:180])

        try:
            await asyncio.Event().wait()
        finally:
            await app.stop()

    asyncio.run(_main())