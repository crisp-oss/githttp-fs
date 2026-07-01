# ToDo

## Now

- [x] Add query parameter to list files starting at given root (eg. `/en/articles`) — sanitize paths to prevent path escape!
- [x] Update commit history to support per-file path history only, using an optional query parameter

## Later

- [ ] Synchronization to GitHub + GitLab (receive hook from GH/GL and mirror repository)
- [ ] Binary API to upload image files over HTTP
- [ ] Ability to serve content over a HTTP Web server (started on the side of HTTP API — from Rust process?)
