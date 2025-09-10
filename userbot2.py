import os
import json
import asyncio
import logging
from pathlib import Path
from dotenv import load_dotenv
from pyrogram import Client
from pyrogram.types import InputMediaPhoto, InputMediaDocument, InputMediaAnimation
import sys

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger("userbot2")

load_dotenv()

def _parse_chat_id(val: str):
    if val is None:
        return None
    s = str(val).strip()
    if "#" in s:
        s = s.split("#", 1)[0].strip()
    if not s:
        return None
    s = s.split()[0]
    return int(s) if s.lstrip("-").isdigit() else s

RAW_SOURCE = os.getenv("SOURCE_CHANNEL") or os.getenv("FORWARD_SOURCE") or os.getenv("SOURCE")
RAW_TARGET = os.getenv("TARGET_CHANNEL") or os.getenv("FORWARD_TARGET") or os.getenv("TARGET")
SOURCE = _parse_chat_id(RAW_SOURCE)
TARGET = _parse_chat_id(RAW_TARGET)
logger.info("parsed SOURCE=%r TARGET=%r", SOURCE, TARGET)

API_ID = int(os.getenv("API_ID", "0"))
API_HASH = os.getenv("API_HASH", "")
SESSION = os.getenv("PYROGRAM_SESSION", "userbot2")

DATA_DIR = Path("data")
DATA_DIR.mkdir(exist_ok=True)
LAST_PATH = DATA_DIR / "last_id.json"
TMP_DIR = DATA_DIR / "tmp"
TMP_DIR.mkdir(parents=True, exist_ok=True)

POLL_INTERVAL = float(os.getenv("POLL_INTERVAL_SEC", "3.0"))
SKIP_VIDEOS = os.getenv("SKIP_VIDEOS", "1") == "1"
HISTORY_BATCH = int(os.getenv("POLL_BATCH", "100"))

app = Client(SESSION, api_id=API_ID, api_hash=API_HASH)


def read_last_id():
    try:
        if LAST_PATH.exists():
            with LAST_PATH.open("r", encoding="utf-8") as f:
                return json.load(f).get("last_id")
    except Exception:
        logger.exception("read_last_id failed")
    return None


def write_last_id(last_id):
    try:
        with LAST_PATH.open("w", encoding="utf-8") as f:
            json.dump({"last_id": last_id}, f)
    except Exception:
        logger.exception("write_last_id failed")


def is_video_message(m):
    if getattr(m, "video", None):
        return True
    doc = getattr(m, "document", None)
    if doc:
        mt = getattr(doc, "mime_type", "") or ""
        if mt.startswith("video"):
            return True
    return False


async def send_fallback(msg):
    """Download media and send appropriate type to TARGET (fallback when copy_message fails)."""
    try:
        fp = await app.download_media(msg, file=TMP_DIR)
        if not fp:
            # no file, send caption/text
            if msg.text or msg.caption:
                return await app.send_message(TARGET, msg.text or msg.caption)
            return None
        # send according to media type
        if getattr(msg, "photo", None):
            return await app.send_photo(TARGET, photo=fp, caption=msg.caption or "")
        if getattr(msg, "animation", None):
            return await app.send_animation(TARGET, animation=fp, caption=msg.caption or "")
        if getattr(msg, "voice", None):
            return await app.send_voice(TARGET, voice=fp, caption=msg.caption or "")
        if getattr(msg, "audio", None):
            return await app.send_audio(TARGET, audio=fp, caption=msg.caption or "")
        if getattr(msg, "sticker", None):
            return await app.send_sticker(TARGET, sticker=fp)
        if getattr(msg, "document", None):
            return await app.send_document(TARGET, document=fp, caption=msg.caption or "")
        # generic fallback
        return await app.send_document(TARGET, document=fp, caption=msg.caption or "")
    finally:
        try:
            if fp:
                Path(fp).unlink(missing_ok=True)
        except Exception:
            pass


async def process_message(msg, processed_group_ids):
    mid = getattr(msg, "id", None)
    logger.info("PROCESS msg id=%s chat=%s from=%s media=%s text=%r",
                mid,
                getattr(msg.chat, "id", None),
                getattr(msg.from_user, "id", None) if getattr(msg, "from_user", None) else getattr(msg.sender_chat, "id", None),
                bool(getattr(msg, "media", None) or getattr(msg, "photo", None) or getattr(msg, "document", None) or getattr(msg, "animation", None)),
                (msg.text or msg.caption or "")[:120])

    # skip videos if configured
    if SKIP_VIDEOS and is_video_message(msg):
        logger.info("SKIP video id=%s", mid)
        return

    # media groups handling (process group once)
    mgid = getattr(msg, "media_group_id", None)
    if mgid:
        if mgid in processed_group_ids:
            return
        processed_group_ids.add(mgid)
        try:
            album = [m async for m in app.get_media_group(msg.chat.id, msg.id)]
        except Exception:
            logger.exception("get_media_group failed for id=%s", mid)
            album = []

        if any(is_video_message(m) for m in album):
            logger.info("SKIP album with video mgid=%s", mgid)
            return

        media_group = []
        for i, m in enumerate(album):
            caption = m.caption if i == 0 else ""
            if getattr(m, "photo", None):
                media_group.append(InputMediaPhoto(m.photo.file_id, caption=caption))
            elif getattr(m, "document", None):
                media_group.append(InputMediaDocument(m.document.file_id, caption=caption))
            elif getattr(m, "animation", None):
                media_group.append(InputMediaAnimation(m.animation.file_id, caption=caption))

        if media_group:
            try:
                res = await app.send_media_group(chat_id=TARGET, media=media_group)
                logger.info("send_media_group ok type=%s id=%s", type(res), getattr(res, "id", None))
                return
            except Exception:
                logger.exception("send_media_group failed, will attempt per-item fallback")
                # fallback individual
                for m in album:
                    try:
                        r = await app.copy_message(chat_id=TARGET, from_chat_id=msg.chat.id, message_ids=m.id)
                        logger.info("copy_message (album item) ok id=%s", getattr(r, "id", None))
                    except Exception:
                        logger.exception("copy_message failed for album item id=%s, fallback send", getattr(m, "id", None))
                        await send_fallback(m)
                return

    # Try to copy_message (preserve media/author). If fails, fallback to send or download/send.
    try:
        res = await app.copy_message(chat_id=TARGET, from_chat_id=msg.chat.id, message_ids=mid)
        logger.info("copy_message ok id=%s repr=%r", getattr(res, "id", None), res)
        return
    except Exception:
        logger.exception("copy_message failed for id=%s, will fallback to send", mid)

    # fallback sends
    if getattr(msg, "photo", None) or getattr(msg, "document", None) or getattr(msg, "animation", None) or getattr(msg, "voice", None) or getattr(msg, "audio", None) or getattr(msg, "sticker", None):
        try:
            r = await send_fallback(msg)
            logger.info("send_fallback result id=%s repr=%r", getattr(r, "id", None), r)
        except Exception:
            logger.exception("send_fallback failed for id=%s", mid)
    elif getattr(msg, "text", None):
        try:
            r = await app.send_message(TARGET, msg.text)
            logger.info("send_message ok id=%s repr=%r", getattr(r, "id", None), r)
        except Exception:
            logger.exception("send_message failed for id=%s", mid)
    else:
        logger.info("unknown message type id=%s, skipping", mid)


async def poll_loop():
    last_id = read_last_id()
    if last_id:
        logger.info("starting from persisted last_id=%s", last_id)
    else:
        # on first run, advance last_id to latest message to avoid bulk-processing history
        try:
            recent = []
            async for m in app.get_chat_history(SOURCE, limit=1):
                recent.append(m)
            if recent:
                last_id = recent[0].id
                write_last_id(last_id)
                logger.info("initialized last_id=%s (will not process existing messages)", last_id)
        except Exception:
            logger.exception("failed to initialize last_id, continuing with last_id=None")

    processed_group_ids = set()

    while True:
        try:
            msgs = []
            async for m in app.get_chat_history(SOURCE, limit=HISTORY_BATCH):
                msgs.append(m)
            if not msgs:
                await asyncio.sleep(POLL_INTERVAL)
                continue

            # get messages sorted oldest->newest
            msgs = list(reversed(msgs))

            new_msgs = []
            for m in msgs:
                if last_id is None or m.id > last_id:
                    new_msgs.append(m)

            if new_msgs:
                logger.info("found %d new messages (last_id=%s -> max_id=%s)", len(new_msgs), last_id, max(m.id for m in new_msgs))
                for m in new_msgs:
                    await process_message(m, processed_group_ids)
                last_id = max(m.id for m in new_msgs)
                write_last_id(last_id)
            await asyncio.sleep(POLL_INTERVAL)
        except Exception:
            logger.exception("polling loop error, sleeping before retry")
            await asyncio.sleep(POLL_INTERVAL)


if __name__ == "__main__":
    async def _main():
        await app.start()
        logger.info("client started; SOURCE=%r TARGET=%r", SOURCE, TARGET)
        # quick dialogs debug (keep for info)
        try:
            dialogs = []
            async for d in app.get_dialogs(limit=30):
                dialogs.append((getattr(d.chat, "id", None), getattr(d.chat, "title", None)))
            logger.info("recent dialogs (id,title): %s", dialogs)
        except Exception:
            logger.exception("failed listing dialogs")

        # Perform a single startup test send to TARGET to verify access/permissions.
        # If it fails, exit immediately to avoid app.stop() / dispatcher/loop races.
        try:
            # try direct fetch first
            tgt_chat = await app.get_chat(TARGET)
            logger.info("get_chat(TARGET) OK id=%s username=%s", getattr(tgt_chat, "id", None), getattr(tgt_chat, "username", None))
            send_target = getattr(tgt_chat, "id", TARGET)
        except Exception:
            logger.exception("get_chat(TARGET) failed; searching dialogs for TARGET")
            # fallback: look through dialogs to find a matching chat by id, username or title
            send_target = None
            try:
                async for d in app.get_dialogs(limit=500):
                    ch = getattr(d, "chat", None)
                    if not ch:
                        continue
                    cid = getattr(ch, "id", None)
                    cun = getattr(ch, "username", None)
                    ctitle = getattr(ch, "title", None)
                    if cid == TARGET or cun == TARGET or ctitle == TARGET or (isinstance(TARGET, str) and ctitle and TARGET in ctitle):
                        send_target = cid
                        logger.info("found TARGET in dialogs -> id=%s title=%r username=%r", cid, ctitle, cun)
                        break
            except Exception:
                logger.exception("searching dialogs failed")

            if send_target is None:
                logger.error("Unable to resolve TARGET (%r) via get_chat or dialogs. Use channel @username or ensure the session is a member. Exiting.", TARGET)
                os._exit(1)

        # now attempt the startup test send
        try:
            sent = await app.send_message(send_target, "userbot startup test — verify posting permission")
            logger.info("startup test send ok id=%s repr=%r", getattr(sent, "id", None), sent)
        except Exception:
            logger.exception("startup test send FAILED — cannot send to resolved target %r. Exiting.", send_target)
            os._exit(1)

        # start polling
        await poll_loop()

    asyncio.run(_main())