import os
import json
import asyncio
import logging
from pathlib import Path
from dotenv import load_dotenv
from pyrogram import Client
from pyrogram.types import InputMediaPhoto, InputMediaDocument, InputMediaAnimation, InputMediaVideo
import sys

#THIS IS THE CHANNEL PARSING MODULE

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

def _sanitize_for_filename(val):
    s = str(val)
    return "".join(c if c.isalnum() or c in "-_" else "_" for c in s)

def collect_source_target_pairs():
    """Collect pairs from env: SOURCE/TARGET, SOURCE1/TARGET1, SOURCE2/TARGET2, ..."""
    pairs = {}
    for k, v in os.environ.items():
        k_up = k.upper()
        if k_up.startswith("SOURCE"):
            suffix = k_up[6:]  # '' or '1' or '2'...
            pairs.setdefault(suffix, {})["source"] = v
        elif k_up.startswith("FORWARD_SOURCE"):
            suffix = k_up[14:]
            pairs.setdefault(suffix, {})["source"] = v
        elif k_up.startswith("TARGET"):
            suffix = k_up[6:]
            pairs.setdefault(suffix, {})["target"] = v
        elif k_up.startswith("FORWARD_TARGET"):
            suffix = k_up[14:]
            pairs.setdefault(suffix, {})["target"] = v

    result = []
    for suffix, d in sorted(pairs.items()):
        src = d.get("source")
        tgt = d.get("target")
        if src and tgt:
            result.append((_parse_chat_id(src), _parse_chat_id(tgt)))
    return result

RAW_PAIRS = collect_source_target_pairs()
if not RAW_PAIRS:
    logger.error("No SOURCE/TARGET pairs found in env (SOURCE, TARGET, SOURCE1, TARGET1, ...). Exiting.")
    os._exit(1)

logger.info("configured SOURCE/TARGET pairs: %r", RAW_PAIRS)

API_ID = int(os.getenv("API_ID", "0"))
API_HASH = os.getenv("API_HASH", "")
SESSION = os.getenv("PYROGRAM_SESSION", "userbot2")

DATA_DIR = Path("data")
DATA_DIR.mkdir(exist_ok=True)
TMP_DIR = DATA_DIR / "tmp"
TMP_DIR.mkdir(parents=True, exist_ok=True)

POLL_INTERVAL = float(os.getenv("POLL_INTERVAL_SEC", "10.0"))
SKIP_VIDEOS = os.getenv("SKIP_VIDEOS", "0") == "1"
HISTORY_BATCH = int(os.getenv("POLL_BATCH", "100"))
# Number of recent messages to restore on first run (0 = skip restoring history).
RESTORE_LIMIT = int(os.getenv("RESTORE_LIMIT", "500"))
RESTORE_ALWAYS = os.getenv("RESTORE_ALWAYS", "0") == "1"

app = Client(SESSION, api_id=API_ID, api_hash=API_HASH)


def read_last_id(last_path: Path):
    try:
        if last_path.exists():
            with last_path.open("r", encoding="utf-8") as f:
                return json.load(f).get("last_id")
    except Exception:
        logger.exception("read_last_id failed for %s", last_path)
    return None


def write_last_id(last_path: Path, last_id):
    try:
        with last_path.open("w", encoding="utf-8") as f:
            json.dump({"last_id": last_id}, f)
    except Exception:
        logger.exception("write_last_id failed for %s", last_path)


def is_video_message(m):
    if getattr(m, "video", None):
        return True
    doc = getattr(m, "document", None)
    if doc:
        mt = getattr(doc, "mime_type", "") or ""
        if mt.startswith("video"):
            return True
    return False


async def send_fallback(msg, target):
    """Download media and send appropriate type to TARGET (fallback when file_id send fails)."""
    fp = None
    try:
        fp = await app.download_media(msg, file_name=str(TMP_DIR))
        if not fp:
            if msg.text or msg.caption:
                return await app.send_message(target, msg.text or msg.caption)
            return None
        if getattr(msg, "photo", None):
            return await app.send_photo(target, photo=fp, caption=msg.caption or "")
        if getattr(msg, "video", None):
            return await app.send_video(target, video=fp, caption=msg.caption or "")
        if getattr(msg, "animation", None):
            return await app.send_animation(target, animation=fp, caption=msg.caption or "")
        if getattr(msg, "voice", None):
            return await app.send_voice(target, voice=fp, caption=msg.caption or "")
        if getattr(msg, "audio", None):
            return await app.send_audio(target, audio=fp, caption=msg.caption or "")
        if getattr(msg, "sticker", None):
            return await app.send_sticker(target, sticker=fp)
        if getattr(msg, "document", None):
            return await app.send_document(target, document=fp, caption=msg.caption or "")
        # generic fallback
        return await app.send_document(target, document=fp, caption=msg.caption or "")
    finally:
        try:
            if fp:
                Path(fp).unlink(missing_ok=True)
        except Exception:
            pass


async def process_message(msg, processed_group_ids, target):
    mid = getattr(msg, "id", None)
    logger.info("PROCESS msg id=%s chat=%s from=%s media=%s text=%r",
                mid,
                getattr(msg.chat, "id", None),
                getattr(msg.from_user, "id", None) if getattr(msg, "from_user", None) else getattr(msg.sender_chat, "id", None),
                bool(getattr(msg, "media", None) or getattr(msg, "photo", None) or getattr(msg, "document", None) or getattr(msg, "animation", None) or getattr(msg, "video", None)),
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
            album = await app.get_media_group(msg.chat.id, msg.id)
        except Exception:
            logger.exception("get_media_group failed for id=%s", mid)
            album = []

        if SKIP_VIDEOS and any(is_video_message(m) for m in album):
            logger.info("SKIP album with video mgid=%s", mgid)
            return

        media_group = []
        for i, m in enumerate(album):
            caption = m.caption if i == 0 else ""
            if getattr(m, "photo", None):
                media_group.append(InputMediaPhoto(m.photo.file_id, caption=caption))
            elif getattr(m, "video", None):
                media_group.append(InputMediaVideo(m.video.file_id, caption=caption))
            elif getattr(m, "document", None):
                media_group.append(InputMediaDocument(m.document.file_id, caption=caption))
            elif getattr(m, "animation", None):
                media_group.append(InputMediaAnimation(m.animation.file_id, caption=caption))

        if media_group:
            try:
                res = await app.send_media_group(chat_id=target, media=media_group)
                logger.info("send_media_group ok type=%s id=%s", type(res), getattr(res, "id", None))
                return
            except Exception:
                logger.exception("send_media_group failed, will attempt per-item fallback")
        
                for m in album:
                    try:
                        if getattr(m, "photo", None):
                            r = await app.send_photo(target, photo=m.photo.file_id, caption=m.caption or "")
                        elif getattr(m, "video", None):
                            r = await app.send_video(target, video=m.video.file_id, caption=m.caption or "")
                        elif getattr(m, "animation", None):
                            r = await app.send_animation(target, animation=m.animation.file_id, caption=m.caption or "")
                        elif getattr(m, "document", None):
                            # if document is actually a video (mime-type), try send_video
                            doc = getattr(m, "document", None)
                            mt = getattr(doc, "mime_type", "") or ""
                            if mt.startswith("video"):
                                r = await app.send_video(target, video=m.document.file_id, caption=m.caption or "")
                            else:
                                r = await app.send_document(target, document=m.document.file_id, caption=m.caption or "")
                        else:
                            r = None
                        logger.info("repost via file_id OK id=%s", getattr(r, "id", None) if r else None)
                    except Exception:
                        logger.exception("repost via file_id failed for album item id=%s, fallback send", getattr(m, "id", None))
                        await send_fallback(m, target)
                return

    
    try:
        if getattr(msg, "photo", None):
            res = await app.send_photo(target, photo=msg.photo.file_id, caption=msg.caption or "")
            logger.info("send_photo via file_id ok id=%s", getattr(res, "id", None))
            return
        if getattr(msg, "video", None):
            res = await app.send_video(target, video=msg.video.file_id, caption=msg.caption or "")
            logger.info("send_video via file_id ok id=%s", getattr(res, "id", None))
            return
        if getattr(msg, "animation", None):
            res = await app.send_animation(target, animation=msg.animation.file_id, caption=msg.caption or "")
            logger.info("send_animation via file_id ok id=%s", getattr(res, "id", None))
            return
        if getattr(msg, "document", None):
            doc = getattr(msg, "document", None)
            mt = getattr(doc, "mime_type", "") or ""
            if mt.startswith("video"):
                res = await app.send_video(target, video=msg.document.file_id, caption=msg.caption or "")
            else:
                res = await app.send_document(target, document=msg.document.file_id, caption=msg.caption or "")
            logger.info("send_document/video via file_id ok id=%s", getattr(res, "id", None))
            return
        if getattr(msg, "voice", None):
            res = await app.send_voice(target, voice=msg.voice.file_id, caption=msg.caption or "")
            logger.info("send_voice via file_id ok id=%s", getattr(res, "id", None))
            return
        if getattr(msg, "audio", None):
            res = await app.send_audio(target, audio=msg.audio.file_id, caption=msg.caption or "")
            logger.info("send_audio via file_id ok id=%s", getattr(res, "id", None))
            return
        if getattr(msg, "sticker", None):
            res = await app.send_sticker(target, sticker=msg.sticker.file_id)
            logger.info("send_sticker via file_id ok id=%s", getattr(res, "id", None))
            return
    except Exception:
        logger.exception("send via file_id failed for id=%s, will fallback to download/send", mid)

    # fallback sends
    if getattr(msg, "photo", None) or getattr(msg, "document", None) or getattr(msg, "animation", None) or getattr(msg, "voice", None) or getattr(msg, "audio", None) or getattr(msg, "sticker", None) or getattr(msg, "video", None):
        try:
            r = await send_fallback(msg, target)
            logger.info("send_fallback result id=%s repr=%r", getattr(r, "id", None), r)
        except Exception:
            logger.exception("send_fallback failed for id=%s", mid)
    elif getattr(msg, "text", None):
        try:
            r = await app.send_message(target, msg.text)
            logger.info("send_message ok id=%s repr=%r", getattr(r, "id", None), r)
        except Exception:
            logger.exception("send_message failed for id=%s", mid)
    else:
        logger.info("unknown message type id=%s, skipping", mid)


async def poll_loop(source, target, last_path: Path):
    last_id = read_last_id(last_path)
    if last_id and not RESTORE_ALWAYS:
        logger.info("starting from persisted last_id=%s for source=%r", last_id, source)
    elif last_id and RESTORE_ALWAYS:
        logger.info("RESTORE_ALWAYS=1 -> forcing history restore for source=%r (ignoring persisted last_id=%s)", source, last_id)
        last_id = None
    else:

        try:
            if RESTORE_LIMIT > 0:
                logger.info("restoring up to %d recent messages for source=%r", RESTORE_LIMIT, source)
        
                recent_msgs = [m async for m in app.get_chat_history(source, limit=RESTORE_LIMIT)]
                if recent_msgs:
                    recent_msgs = list(reversed(recent_msgs))  
                    restoration_group_ids = set()
                    for m in recent_msgs:
                        try:
                            await process_message(m, restoration_group_ids, target)
                        except Exception:
                            logger.exception("processing restored message id=%s failed", getattr(m, "id", None))
                    last_id = max(m.id for m in recent_msgs)
                    write_last_id(last_path, last_id)
                    logger.info("restored %d messages, last_id=%s for source=%r", len(recent_msgs), last_id, source)
                else:
                    logger.info("no messages found while restoring for source=%r", source)
            else:
                recent = []
                async for m in app.get_chat_history(source, limit=1):
                    recent.append(m)
                if recent:
                    last_id = recent[0].id
                    write_last_id(last_path, last_id)
                    logger.info("initialized last_id=%s for source=%r (will not process existing messages)", last_id, source)
        except Exception:
            logger.exception("failed to initialize/restore history for source=%r, continuing with last_id=None", source)

    processed_group_ids = set()

    while True:
        try:
            msgs = []
            async for m in app.get_chat_history(source, limit=HISTORY_BATCH):
                msgs.append(m)
            if not msgs:
                await asyncio.sleep(POLL_INTERVAL)
                continue

            msgs = list(reversed(msgs))

            new_msgs = []
            for m in msgs:
                if last_id is None or m.id > last_id:
                    new_msgs.append(m)

            if new_msgs:
                logger.info("found %d new messages for source=%r (last_id=%s -> max_id=%s)", len(new_msgs), source, last_id, max(m.id for m in new_msgs))
                for m in new_msgs:
                    await process_message(m, processed_group_ids, target)
                last_id = max(m.id for m in new_msgs)
                write_last_id(last_path, last_id)
            await asyncio.sleep(POLL_INTERVAL)
        except Exception:
            logger.exception("polling loop error for source=%r, sleeping before retry", source)
            await asyncio.sleep(POLL_INTERVAL)


if __name__ == "__main__":
    async def _main():
        await app.start()
        logger.info("client started; will monitor %d pair(s)", len(RAW_PAIRS))
        try:
            dialogs = []
            async for d in app.get_dialogs(limit=30):
                dialogs.append((getattr(d.chat, "id", None), getattr(d.chat, "title", None)))
            logger.info("recent dialogs (id,title): %s", dialogs)
        except Exception:
            logger.exception("failed listing dialogs")

        tasks = []
        for source, target in RAW_PAIRS:
            # create unique last path per source
            last_file = DATA_DIR / f"last_{_sanitize_for_filename(source)}.json"

            try:
                tgt_chat = await app.get_chat(target)
                logger.info("get_chat(TARGET) OK for target=%r id=%s username=%s", target, getattr(tgt_chat, "id", None), getattr(tgt_chat, "username", None))
                send_target = getattr(tgt_chat, "id", target)
            except Exception:
                logger.exception("get_chat(TARGET) failed for %r; searching dialogs for TARGET", target)
                send_target = None
                try:
                    async for d in app.get_dialogs(limit=500):
                        ch = getattr(d, "chat", None)
                        if not ch:
                            continue
                        cid = getattr(ch, "id", None)
                        cun = getattr(ch, "username", None)
                        ctitle = getattr(ch, "title", None)
                        if cid == target or cun == target or ctitle == target or (isinstance(target, str) and ctitle and target in ctitle):
                            send_target = cid
                            logger.info("found TARGET in dialogs -> id=%s title=%r username=%r", cid, ctitle, cun)
                            break
                except Exception:
                    logger.exception("searching dialogs failed for target=%r", target)

                if send_target is None:
                    logger.error("Unable to resolve TARGET (%r) via get_chat or dialogs. Skipping this pair.", target)
                    continue

            try:
                sent = await app.send_message(send_target, f"userbot startup test — verify posting permission for source={source} -> target={target}")
                logger.info("startup test send ok for pair %r -> %r id=%s", source, target, getattr(sent, "id", None))
            except Exception:
                logger.exception("startup test send FAILED for pair %r -> %r. Skipping this pair.", source, target)
                continue

            t = asyncio.create_task(poll_loop(source, send_target, last_file))
            tasks.append(t)

        if not tasks:
            logger.error("No valid SOURCE/TARGET pairs started. Exiting.")
            os._exit(1)

        await asyncio.gather(*tasks)

    asyncio.run(_main())