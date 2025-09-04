import asyncio
import csv
import os
from pathlib import Path
from typing import Iterable

from dotenv import load_dotenv
from pyrogram import Client
from pyrogram.enums import ChatType

ROOT = Path(__file__).resolve().parents[1]
CSV_ALL = ROOT / "src" / "chats_export.csv"          # all dialogs
CSV_CHANNELS = ROOT / "src" / "channels_seed.csv"    # channels only (for Rust import)

# Config
INCLUDE_SUPERGROUPS_IN_CHANNELS = False  # set True to include supergroups in channels_seed.csv

def norm_username(u: str | None) -> str:
    return "" if not u else u.lstrip("@")

def display_title(chat) -> str:
    if chat.type in (ChatType.PRIVATE, ChatType.BOT):
        first = chat.first_name or ""
        last = chat.last_name or ""
        name = (first + " " + last).strip()
        return name or (chat.username or "Private")
    return chat.title or "Untitled"

async def main() -> None:
    load_dotenv()
    api_id = int(os.environ["API_ID"])
    api_hash = os.environ["API_HASH"]

    CSV_ALL.parent.mkdir(parents=True, exist_ok=True)

    rows_all: list[tuple[str, int, str, str]] = []
    rows_channels: list[tuple[int, str, str]] = []
    seen_ids: set[int] = set()

    async with Client("userbot", api_id=api_id, api_hash=api_hash) as app:
        async for dlg in app.get_dialogs():
            chat = dlg.chat
            cid = int(chat.id)
            if cid in seen_ids:
                continue
            seen_ids.add(cid)

            ctype = chat.type  # ChatType enum
            title = display_title(chat)
            username = norm_username(getattr(chat, "username", None))

            # Store all dialogs
            rows_all.append((ctype.value, cid, title, username))

            # Prepare channels_seed.csv (channels only, or include supergroups if enabled)
            if ctype == ChatType.CHANNEL or (INCLUDE_SUPERGROUPS_IN_CHANNELS and ctype == ChatType.SUPERGROUP):
                rows_channels.append((cid, title, username))

    # Sort
    rows_all.sort(key=lambda r: (r[0], r[2].lower()))
    rows_channels.sort(key=lambda r: r[1].lower())

    # Write all dialogs CSV: type;id;title;username
    with CSV_ALL.open("w", encoding="utf-8", newline="") as f:
        w = csv.writer(f, delimiter=";")
        w.writerow(["type", "id", "title", "username"])
        for ctype, cid, title, username in rows_all:
            w.writerow([ctype, cid, title, username])

    # Write channels seed CSV compatible with your Rust importer: id;title;username
    with CSV_CHANNELS.open("w", encoding="utf-8", newline="") as f:
        w = csv.writer(f, delimiter=";")
        w.writerow(["id", "title", "username"])
        for cid, title, username in rows_channels:
            w.writerow([cid, title, username])

    print(f"Wrote {len(rows_all)} dialogs to {CSV_ALL}")
    print(f"Wrote {len(rows_channels)} channels to {CSV_CHANNELS} (INCLUDE_SUPERGROUPS_IN_CHANNELS={INCLUDE_SUPERGROUPS_IN_CHANNELS})")

if __name__ == "__main__":
    asyncio.run(main())