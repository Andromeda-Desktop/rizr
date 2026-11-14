import os

from sqlalchemy import create_engine, text

engine = create_engine(os.environ["DATABASE_URL"])


def add_item(name):
    with engine.begin() as conn:
        return conn.execute(text("INSERT INTO items (name) VALUES (:n) RETURNING id"), {"n": name}).scalar_one()


def list_items():
    with engine.connect() as conn:
        return [row.name for row in conn.execute(text("SELECT name FROM items ORDER BY id"))]
