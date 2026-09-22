# ADR-0001: Data sink and transform for inactive email identities

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: WIP.
- **Status**: Draft.

## Context

I have changed my name multiple times, used multiple aliases, and operated through multiple legal entities. The majority of these had unique email addresses with Google (Gmail or Google Workspace), Fastmail, Proton, and iCloud. I need a consistent, reliable way to connect to these email providers to pull all emails and attachments, convert them to a standard format (file name and folder structure and taxonomy design).

## Decision

### Stack / Format

- Rust has a robust, established community for email connectivity crates. Will likely use [async-imap](https://github.com/chatmail/async-imap).
- Create a thin CLI wrapper to authenticate the email service(s) and invoke the sink/transform operations.

### Features

- Authenticate with Google, Fastmail, iCloud, and Proton.
- Invoke batch operations.
  - To sink all emails and attachments from the respective server.
  - To transform MBOX and EML files to Markdown files.
  - To transform file names and folder structures to match design.
- Progress bar or % for long-running operations.
- Support for application keys (or other mechanisms to avoid user/pass authentication).
 
### Interface

```
$ pigeon email authenticate first.last@example.com --alias first-last
$ pigeon email list-identities
$ pigeon email sink first-last --directory /tmp/first
$ pigeon email transform --input /tmp/first --output /tmp/second --normalize --mbox-to-markdown
```

### File Naming Scheme and Folder Structure

- `first.last@example.com` becomes `first-last-example-com` directory.
- Markdown email-representations follow the naming scheme: `YYYY-MM-DD-${sanitized-subject}.md`.
  - The subject line is sanitized of non English alphanumeric tokens and spaces.
  - File name is strictly Kebab and lower case.
- Attachments follow the same naming scheme for the email with the original file name suffixed.

For example:

```
jane-doe-gmail-com/2024-01-26-hello-world.md
jane-doe-gmail-com/attachments/2024-01-26-hello-world-bingo-sheet.pdf
```

### Frontmatter and Taxonomy 

To-do.
