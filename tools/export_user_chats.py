import os
import csv
import asyncio
from telethon import TelegramClient
from telethon.tl.types import Channel, Chat, User
from dotenv import load_dotenv

load_dotenv()  # loads .env if present

API_ID = int(os.environ.get("API_ID", "0"))
API_HASH = os.environ.get("API_HASH", "")
SESSION = os.environ.get("TELETHON_SESSION", "user")  # optional custom session name
OUT_PATH = os.path.join(os.path.dirname(__file__), "..", "src", "chats_export.csv")

if API_ID == 0 or not API_HASH:
    raise SystemExit("Set API_ID and API_HASH in environment or .env")

def dialog_type(entity):
    if isinstance(entity, Channel):
        # broadcast channels vs supergroups
        if getattr(entity, "broadcast", False):
            return "channel"
        if getattr(entity, "megagroup", False):
            return "supergroup"
        return "channel"
    if isinstance(entity, Chat):
        return "group"
    if isinstance(entity, User):
        return "private" if not getattr(entity, "bot", False) else "bot"
    return "channel"

def export_chat_id(entity, typ):
    # For channels/supergroups Telegram Bot API style id is -100{channel.id}
    try:
        eid = int(entity.id)
    except Exception:
        eid = 0
    if typ in ("channel", "supergroup"):
        return f"-100{eid}"
    return str(eid)

async def main():
    client = TelegramClient(SESSION, API_ID, API_HASH)
    await client.start()  # will prompt for phone/code if no session
    rows = []
    async for dialog in client.iter_dialogs(limit=None):
        ent = dialog.entity
        typ = dialog_type(ent)
        cid = export_chat_id(ent, typ)
        # title and username
        title = getattr(ent, "title", None)
        if not title:
            # user fallback
            first = getattr(ent, "first_name", "") or ""
            last = getattr(ent, "last_name", "") or ""
            title = (first + " " + last).strip() or getattr(ent, "username", "") or ""
        username = getattr(ent, "username", "") or ""
        # folder column kept empty for compatibility
        rows.append((typ, cid, title, username, ""))

    # sort by title (case-insensitive) to mimic original
    rows.sort(key=lambda r: r[2].lower())

    # ensure output dir exists and write CSV with semicolons
    out_dir = os.path.dirname(OUT_PATH)
    os.makedirs(out_dir, exist_ok=True)
    with open(OUT_PATH, "w", newline="", encoding="utf-8") as f:
        writer = csv.writer(f, delimiter=";", quoting=csv.QUOTE_MINIMAL)
        # header (original had 4 columns; add folder as 5th for parity with existing rows)
        writer.writerow(["type", "id", "title", "username", "folder"])
        for r in rows:
            writer.writerow(r)

    print(f"Wrote {len(rows)} rows to {OUT_PATH}")
    await client.disconnect()

if __name__ == "__main__":
    asyncio.run(main())