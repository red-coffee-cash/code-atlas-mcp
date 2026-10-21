from flask import Flask, request

from app.db import Database, NoteRepo, UserRepo
from app.notes import NoteService

app = Flask(__name__)
db = Database("notes.db")
service = NoteService(NoteRepo(db), UserRepo(db))


@app.route("/notes/<int:note_id>")
def show_note(note_id):
    return {"note": service.get_note(request.headers, note_id)}


@app.route("/search")
def search():
    return {"results": service.search_notes(request.headers, request.args.get("q", ""))}
