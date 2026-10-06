# std.mail rules

- A character sends a letter (subject 1 to 32 bytes, body up to 200) to
  another character, optionally attaching gold and part of one bag stack.
  Postage is 5 gold per letter and is burned.
- Sending, taking attachments, and deleting are economy commands. Sending
  debits the gold and postage and takes the items in one transaction, after
  checking the recipient's mailbox has room, so a refused letter costs
  nothing and an accepted letter is always delivered.
- A mailbox holds 50 letters; letters to a full mailbox are refused.
- The recipient takes a letter's gold and items into its bag all at once;
  if they do not fit, nothing moves. A letter can be deleted only once
  nothing is attached.
- A recipient present in the cell is told when a letter arrives.
- Flag `attachments` (default on) gates gold and items; plain letters still
  go when it is off.
- Other modules deliver letters (auction returns and payments) through the
  contract's `Mailboxes::deliver`, inside their own commands.
- Sending (`SendMail`), taking attachments (`TakeMail`), and deleting
  (`DeleteLetter`) answer the requester with a `MailResult` (its `request`
  echoed, a `MAIL_*` reason code) and a fresh first `MailPage`. `SendMail`
  names an attached item explicitly (`has_item`). The first messages
  (`Send`, `TakeAttachments`, `DeleteMail`, `ListMail`, `MailList`) are
  retired in the registry: their ids are never reused.
- `ListMailPage` answers with one `MailPage` of up to 6 letters, newest
  first, from an offset, with the mailbox total; each entry carries its
  subject, read flag, and attachments. Reading a letter marks it read.

Recipients are characters by id. Whether a character exists, and mailboxes
of characters in other cells, are the business of the account and social
services (plan section 10); here a letter is held in the cell's mailboxes
and recorded through the command's outcome.

A sub-feature switched off by its flag is refused as not allowed;
`FeatureDisabled` is reserved for the whole module being off (clients
grey out the module on it).
