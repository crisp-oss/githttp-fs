# ToDo

## Now

- [x] Add options to githttp-fs in read file to seek from start line to end line (array) + maximum lines to seek, can be combined
  - [x] Seek start should be a fuzzy match, at start of line.
  - [x] Make 'seek_from_line_starts_with' multi-format: can be an array, where 'seek_to_line_starts_with' can be set up to repeat the matched 'from' (eg. with $from meta argument)
- [ ] Add a batch file read route to githttp-fs (returning empty or file content — with a batch size safety maximum)
  - [ ] Re-use seek options in the batch read file route too.

## Later

- [ ] Synchronization to GitHub + GitLab (receive hook from GH/GL and mirror repository)
- [ ] Binary API to upload image files over HTTP
- [ ] Ability to serve content over a HTTP Web server (started on the side of HTTP API — from Rust process?)

## Done

- [x] Add query parameter to list files starting at given root (eg. `/en/articles`) — sanitize paths to prevent path escape!
- [x] Update commit history to support per-file path history only, using an optional query parameter
- [x] Add paging in file list route + maximum depth
- [x] Review whole code for security issues & performance issues on large/deep repositories w/ Fable
- [x] Ask Fable to improve Rust code quality, following Rust coding best practices and add a lot of comments for humans to understand the code better and justify design decisions
