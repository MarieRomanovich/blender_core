import os
import asyncio
import json
import logging
from pathlib import Path
from typing import Dict, Any, List, Optional

from telethon import TelegramClient, events
from telethon.tl.types import Message
import importlib

from dotenv import load_dotenv

load_dotenv()

LOG = logging.getLogger("userbot3")
logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")

API_ID = int(os.environ.get("API_ID", "2040"))
API_HASH = os.environ.get("API_HASH", "b18441a1ff607e10a989891a5462e627")
SESSION = os.environ.get("SESSION", "main_session")
DEFAULT_CHAT = int(os.environ.get("TARGET_CHAT", "-1002820338492"))
TOPIC_MAP_PATHS = [Path("src/topic_map.json"), Path("topic_map.json"), Path("data/topic_map.json")]
SCAN_LIMIT = int(os.environ.get("SCAN_LIMIT", "2000"))

client = TelegramClient(SESSION, API_ID, API_HASH)

TOPIC_MAPPINGS: List[Dict[str, Any]] = []
STARTER_INDEX: Dict[Optional[int], Dict[str, Any]] = {}


def load_topic_map() -> List[Dict[str, Any]]:
    for p in TOPIC_MAP_PATHS:
        if p.exists():
            try:
                j = json.loads(p.read_text(encoding="utf-8"))
                if isinstance(j, dict):
                    if "mappings" in j and isinstance(j["mappings"], list):
                        return j["mappings"]
                    return list(j.values())
                if isinstance(j, list):
                    return j
            except Exception as e:
                LOG.error("Failed to load %s: %s", p, e)
    LOG.warning("No topic_map.json found; continuing with empty map")
    return []


def normalize_mapping(m: Any) -> Dict[str, Any]:
    """
    Accept either:
      { "target": {"name": "Title", "topic_id": 123} }
      or { "name": "...", "topic_id": 123 }
      or simple {"name": "...}
    Return dict with optional keys 'name' (str) and 'topic_id' (int).
    """
    if isinstance(m, dict) and "target" in m and isinstance(m["target"], dict):
        t = m["target"]
    elif isinstance(m, dict):
        t = m
    else:
        return {"name": str(m)}
    out: Dict[str, Any] = {}
    if "topic_id" in t:
        try:
            out["topic_id"] = int(t["topic_id"])
        except Exception:
            pass
    if "name" in t and isinstance(t["name"], str):
        out["name"] = t["name"].strip()
    return out


async def try_load_forum_topics(chat: int | str, limit: int = 200) -> Dict[int, str]:
    """
    If Telethon exposes a GetForumTopics RPC, call it and return mapping topic_id->title.
    Otherwise return empty dict.
    """
    candidates = [
        ("telethon.tl.functions.messages", "GetForumTopicsRequest"),
        ("telethon.tl.functions.messages", "GetForumTopics"),
        ("telethon.tl.functions.channels", "GetForumTopicsRequest"),
        ("telethon.tl.functions.channels", "GetForumTopics"),
    ]
    rpc = None
    for mod_name, cls_name in candidates:
        try:
            mod = importlib.import_module(mod_name)
            rpc = getattr(mod, cls_name)
            break
        except Exception:
            rpc = None
    if rpc is None:
        return {}
    ent = await client.get_entity(chat)
    try:
        # try keyword form and positional fallback
        try:
            resp = await client(rpc(peer=ent, q="", offset_date=0, offset_id=0, offset_peer=ent, limit=limit))
        except TypeError:
            resp = await client(rpc(ent, "", 0, 0, ent, limit))
        raw = getattr(resp, "topics", None) or getattr(resp, "forum_topics", None) or []
        out = {}
        for t in raw:
            tid = getattr(t, "id", None)
            title = getattr(t, "title", None) or getattr(t, "label", None) or None
            if tid is not None and title:
                out[int(tid)] = title
        LOG.info("Loaded %d forum topics via RPC", len(out))
        return out
    except Exception as e:
        LOG.debug("Forum RPC call failed: %s", e)
        return {}


async def build_starter_index(chat: int | str, limit: int = SCAN_LIMIT, persist: bool = True) -> Dict[Optional[int], Dict[str, Any]]:
    
    ent = await client.get_entity(chat)
    index: Dict[Optional[int], Dict[str, Any]] = {}
    heur = {}
    cnt = 0
    async for m in client.iter_messages(ent, limit=limit):
        cnt += 1
        if not isinstance(m, Message):
            continue
        mid = getattr(m, "id", None)
        tid = getattr(m, "message_thread_id", None)
        text = (m.message or "").strip() if getattr(m, "message", None) else ""
        if tid:
            info = index.get(tid)
            if info is None:
                index[tid] = {"starter_id": None, "starter_text": text, "min_id": mid}
            else:
                if mid is not None and (info.get("min_id") is None or mid < info.get("min_id")):
                    info["min_id"] = mid
                    info["starter_text"] = text
            if mid and tid == mid:
                index[tid]["starter_id"] = mid
        else:
            if text.startswith("#") and len(text) < 300:
                heur[text] = {"starter_id": mid, "starter_text": text}
    for tid, info in list(index.items()):
        if info.get("starter_id") is None:
            info["starter_id"] = info.get("min_id") or tid
    if heur:
        index.setdefault(None, {"heuristic": []})
        for h in heur.values():
            index[None]["heuristic"].append(h)
    LOG.info("Index built: scanned=%d threads=%d heuristic=%d", cnt, len([k for k in index if k is not None]), len(index.get(None, {}).get("heuristic", [])))

    # persist index for debugging / further processing
    if persist:
        Path("data").mkdir(exist_ok=True)
        try:
            chat_id = getattr(ent, "id", str(chat))
            path = Path(f"data/forum_starters_{chat_id}.json")
            serial = {str(k): v for k, v in index.items()}
            path.write_text(json.dumps(serial, ensure_ascii=False, indent=2), encoding="utf-8")
            LOG.info("Starter index saved to %s", path)
        except Exception as e:
            LOG.exception("Failed to persist starter index: %s", e)
    return index


async def backup_history(chat: int | str, limit: Optional[int] = None):
   
    ent = await client.get_entity(chat)
    Path("data").mkdir(exist_ok=True)
    chat_id = getattr(ent, "id", str(chat))
    out_path = Path(f"data/history_{chat_id}.jsonl")
    written = 0
    async for m in client.iter_messages(ent, limit=limit or SCAN_LIMIT):
        try:
            obj = {
                "id": getattr(m, "id", None),
                "date": getattr(m, "date", None).isoformat() if getattr(m, "date", None) else None,
                "thread_id": getattr(m, "message_thread_id", None),
                "from_id": getattr(m, "from_id", None).user_id if getattr(m, "from_id", None) else None,
                "text": (m.message or "")[:10000],
            }
            out_path.write_text((out_path.read_text(encoding="utf-8") if out_path.exists() else "") + json.dumps(obj, ensure_ascii=False) + "\n", encoding="utf-8")
            written += 1
        except Exception:
            LOG.exception("Failed to write message %s", getattr(m, "id", None))
    LOG.info("Backed up %d messages to %s", written, out_path)
    return out_path


def match_by_mappings(thread_id: Optional[int], starter_text: str, mappings: List[Dict[str, Any]]) -> Optional[Dict[str, Any]]:
    
    txt = (starter_text or "").lower()
    # check ids first
    if thread_id is not None:
        for m in mappings:
            if "topic_id" in m and m["topic_id"] is not None and int(m["topic_id"]) == int(thread_id):
                return m
    # then by name
    for m in mappings:
        if "name" in m and m["name"]:
            if m["name"].lower() in txt:
                return m
    return None


@client.on(events.NewMessage(chats=DEFAULT_CHAT))
async def on_new(ev: events.NewMessage.Event):
    m = ev.message
    chat = ev.chat_id or DEFAULT_CHAT
    tid = getattr(m, "message_thread_id", None)
    reply = getattr(m, "reply_to", None)
    if reply and getattr(reply, "reply_to_msg_id", None):
        tid = reply.reply_to_msg_id
    starter_text = ""
    if tid is not None:
        try:
            starter = await client.get_messages(chat, ids=int(tid))
            starter_text = getattr(starter, "message", "") if starter else ""
        except Exception:
            starter_text = ""
    if not starter_text:
        starter_text = (m.message or "") or ""
    found = match_by_mappings(tid, starter_text, TOPIC_MAPPINGS)
    if not found and starter_text.startswith("#"):
        found = match_by_mappings(None, starter_text, TOPIC_MAPPINGS)
    if found:
        out = {"chat": chat, "msg_id": m.id, "thread_id": tid, "mapping": found, "text": (m.message or "")[:400]}
        LOG.info("Matched -> %s", found)
        Path("data").mkdir(exist_ok=True)
        with open(Path("data/notifications.log"), "a", encoding="utf-8") as f:
            f.write(json.dumps(out, ensure_ascii=False) + "\n")
    else:
        LOG.debug("No mapping for msg=%s thread=%s text=%r", m.id, tid, (m.message or "")[:80])


async def periodic_rescan(chat: int | str, interval: int = 3600):
    """
    Periodically rescans history and rewrites starter index and backup files.
    Use small interval for testing (e.g. 60).
    """
    while True:
        try:
            LOG.info("Periodic rescan starting for %s", chat)
            new_index = await build_starter_index(chat, limit=SCAN_LIMIT, persist=True)
            global STARTER_INDEX
            STARTER_INDEX = new_index
            await backup_history(chat, limit=SCAN_LIMIT)
        except Exception:
            LOG.exception("Periodic rescan failed")
        await asyncio.sleep(interval)


async def main():
    global TOPIC_MAPPINGS, STARTER_INDEX
    raw = load_topic_map()
    TOPIC_MAPPINGS = [normalize_mapping(r) for r in raw]
    await client.start()
    LOG.info("Client started as %s", await client.get_me())

    rpc_topics = await try_load_forum_topics(DEFAULT_CHAT, limit=1000)
    if rpc_topics:
    
        for m in TOPIC_MAPPINGS:
            if "name" in m and "topic_id" not in m:
                for tid, title in rpc_topics.items():
                    if m["name"].lower() in title.lower():
                        m["topic_id"] = int(tid)
                        break

    STARTER_INDEX = await build_starter_index(DEFAULT_CHAT, limit=SCAN_LIMIT, persist=True)

    # backup history once on start
    await backup_history(DEFAULT_CHAT, limit=SCAN_LIMIT)

    asyncio.create_task(periodic_rescan(DEFAULT_CHAT, interval=int(os.environ.get("RESCAN_INTERVAL", "3600"))))

    LOG.info("Listening for new messages in %s ...", DEFAULT_CHAT)
    await client.run_until_disconnected()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        LOG.info("Stopping")
