"""Writes test_data/sonar_fixture.db: a made-up database in the layout the Sonar importer reads
(`configs`: name, vad, is_preset, data, game_ids). No SteelSeries data; every profile is invented.
Run again after changing it: python test_data/make_sonar_fixture.py
"""
import json
import os
import sqlite3

PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), "sonar_fixture.db")


def eq(gain, *filters):
  peq = {"enabled": True}
  for i, (kind, freq, db, q) in enumerate(filters, 1):
    peq[f"filter{i}"] = {"enabled": True, "type": kind, "frequency": freq, "gain": db, "qFactor": q}
  return json.dumps({"generalGain": gain, "parametricEQ": peq})


# (name, vad, is_preset, data, game_ids); vad: 1 game, 2 chat, 3 mic, 4 media, 5 aux.
ROWS = [
  ("Fixture Shooter", 1, 1, eq(-1.5, ("lowShelving", 90.0, -2.0, 0.7), ("peakingEQ", 3200.0, 4.5, 1.3)),
   "1_100001, 15_Fixture Shooter"),
  ("Fixture Racing", 1, 1, eq(0.0, ("peakingEQ", 60.0, 3.0, 1.0)), "1_100002"),
  ("My game tweak", 1, 0, eq(0.0, ("highShelving", 9000.0, 2.0, 0.7)), ""),
  ("Fixture Chat", 2, 1, eq(0.0, ("highPass", 110.0, 0.0, 0.7)), ""),
  ("Fixture Voice", 3, 1, eq(1.0, ("peakingEQ", 250.0, -3.0, 1.4), ("notchFilter", 50.0, 0.0, 8.0)), ""),
  ("Fixture Music", 4, 1, eq(0.0, ("lowPass", 18000.0, 0.0, 0.7)), ""),
  ("No EQ block", 5, 1, json.dumps({"generalGain": 0.0}), ""),
  ("Broken JSON", 1, 1, "{not json", ""),
]

if os.path.exists(PATH):
  os.remove(PATH)
db = sqlite3.connect(PATH)
db.execute(
  "CREATE TABLE configs (id TEXT PRIMARY KEY, name TEXT NOT NULL, vad INTEGER NOT NULL, data TEXT NOT NULL,"
  " is_preset INTEGER NOT NULL, game_ids TEXT NOT NULL DEFAULT '')"
)
db.executemany(
  "INSERT INTO configs (id, name, vad, is_preset, data, game_ids) VALUES (?, ?, ?, ?, ?, ?)",
  [(f"fixture-{i}", *row) for i, row in enumerate(ROWS)],
)
db.commit()
db.close()
print(f"wrote {PATH} ({len(ROWS)} rows)")
