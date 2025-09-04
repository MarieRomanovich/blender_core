import os
from pathlib import Path

from dotenv import load_dotenv
from pyrogram import Client
from pyrogram.enums import ChatType

CSV_PATH = Path(__file__).resolve().parents[1] / "src" / "channels_seed.csv"
INCLUDE_SUPERGROUPS = False  # set True if you also want supergroups

def norm_username(u: str | None) -> str:
    if not u:
        return ""
    return u.lstrip("@")

async def run() -> None:
    load_dotenv()
    api_id = int(os.environ["API_ID"])
    api_hash = os.environ["API_HASH"]

    CSV_PATH.parent.mkdir(parents=True, exist_ok=True)

    async with Client("userbot", api_id=api_id, api_hash=api_hash) as app:
        seen = set()
        rows: list[tuple[int, str, str]] = []

        async for dlg in app.get_dialogs():
            chat = dlg.chat
            if chat.type == ChatType.CHANNEL or (INCLUDE_SUPERGROUPS and chat.type == ChatType.SUPERGROUP):
                cid = int(chat.id)
                if cid in seen:
                    continue
                seen.add(cid)
                title = chat.title or "Untitled"
                username = norm_username(chat.username)
                rows.append((cid, title, username))

    # Sort by title (case-insensitive)
    rows.sort(key=lambda r: r[1].lower())

    # Write CSV: id;title;username (username optional)
    lines = []
    lines.append("# id;title;username")
    lines.append("# username is optional (without @). One channel per line.")
    for cid, title, username in rows:
        lines.append(f"{cid};{title};{username}")
    CSV_PATH.write_text("\n".join(lines), encoding="utf-8")

    print(f"Wrote {len(rows)} channels to {CSV_PATH}")

if __name__ == "__main__":
    import asyncio
    asyncio.run(run())