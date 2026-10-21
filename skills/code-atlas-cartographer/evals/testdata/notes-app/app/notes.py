from app.auth import current_user


class NoteService:
    def __init__(self, notes, users):
        self.notes = notes
        self.users = users

    def get_note(self, headers, note_id):
        user = current_user(headers, self.users)
        if user is None:
            raise PermissionError("login required")
        note = self.notes.find(note_id)
        return note

    def search_notes(self, headers, term):
        user = current_user(headers, self.users)
        if user is None:
            raise PermissionError("login required")
        return self.notes.search(user[0], term)

    def rename(self, headers, repo, record, name):
        user = current_user(headers, self.users)
        if user is None or not user[2]:
            raise PermissionError("admin only")
        record["name"] = name
        repo.save(record)
