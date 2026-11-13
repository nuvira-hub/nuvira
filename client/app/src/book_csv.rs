//! Address book to and from CSV files: a backup, a spreadsheet, or the list of
//! another tool once the computers have their NuviraHub IDs.
//!
//! Columns: `id` (the NuviraHub ID, 9 digits), `name`, `company`, `contact`,
//! `email`, `phone`, `group`, `tags` (separated by `;` or `|`), `notes`,
//! `password`. The header names them, in any order, in English or in one of the
//! app's languages; other columns are ignored. Commas or semicolons separate the
//! fields (Excel with an Italian, French, German or Spanish locale writes
//! semicolons). The export leaves the device passwords out: a CSV file is plain
//! text.

use crate::address_book::EntryUi;

/// The fields of an entry, as the export writes them.
pub const COLUMNS: [&str; 9] = ["id", "name", "company", "contact", "email", "phone", "group", "tags", "notes"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Id,
    Name,
    Company,
    Contact,
    Email,
    Phone,
    Group,
    Tags,
    Notes,
    Password,
}

/// The column a header names, in the app's languages.
fn field(header: &str) -> Option<Field> {
    let h: String = header
        .trim()
        .trim_start_matches('\u{feff}')
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect();
    Some(match h.as_str() {
        "id" | "deviceid" | "nuvirahubid" | "nuviraid" | "iddispositivo" | "identifiant" | "geräteid" | "geraeteid"
        | "iddelequipo" | "iddeldispositivo" => Field::Id,
        "name" | "nome" | "nom" | "nombre" | "devicename" | "computer" | "computername" | "pc" | "hostname"
        | "alias" => Field::Name,
        "company" | "azienda" | "cliente" | "customer" | "client" | "société" | "societe" | "entreprise" | "firma"
        | "kunde" | "empresa" => Field::Company,
        "contact" | "contatto" | "referente" | "kontakt" | "ansprechpartner" | "contacto" => Field::Contact,
        "email" | "mail" | "courriel" | "correo" | "correoelectrónico" | "correoelectronico" => Field::Email,
        "phone" | "telefono" | "tel" | "téléphone" | "telephone" | "telefon" | "teléfono" => Field::Phone,
        "group" | "gruppo" | "groupe" | "gruppe" | "grupo" | "folder" | "cartella" => Field::Group,
        "tags" | "tag" | "etichette" | "étiquettes" | "etiquettes" | "schlagwörter" | "schlagworter" | "etiquetas" => {
            Field::Tags
        }
        "notes" | "note" | "notizen" | "notas" | "description" | "descrizione" => Field::Notes,
        "password" | "passwort" | "motdepasse" | "contraseña" | "contrasena" => Field::Password,
        _ => return None,
    })
}

/// A row of the file that could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// Line in the file, from 1 (the header is line 1).
    pub line: usize,
    /// Translation key of the reason.
    pub reason: &'static str,
}

/// The entries of a CSV file (`book_id` and `id` left at 0), and the rows skipped.
pub fn parse(text: &str) -> Result<(Vec<(usize, EntryUi)>, Vec<Skipped>), &'static str> {
    let text = text.trim_start_matches('\u{feff}');
    let first = text.lines().next().unwrap_or("");
    let separator = if first.matches(';').count() > first.matches(',').count() { ';' } else { ',' };
    let rows = records(text, separator);
    let Some((_, header)) = rows.first() else { return Err("book_csv.empty") };
    let columns: Vec<Option<Field>> = header.iter().map(|h| field(h)).collect();
    if !columns.contains(&Some(Field::Id)) {
        return Err("book_csv.no_id_column");
    }
    let mut entries = Vec::new();
    let mut skipped = Vec::new();
    for (line, row) in rows.into_iter().skip(1) {
        if row.iter().all(|v| v.trim().is_empty()) {
            continue;
        }
        let mut e = EntryUi::default();
        for (value, column) in row.iter().zip(&columns) {
            let v = value.trim().to_string();
            match column {
                Some(Field::Id) => e.device_id = v.chars().filter(|c| c.is_ascii_digit()).collect(),
                Some(Field::Name) => e.name = v,
                Some(Field::Company) => e.company = v,
                Some(Field::Contact) => e.contact = v,
                Some(Field::Email) => e.email = v,
                Some(Field::Phone) => e.phone = v,
                Some(Field::Group) => e.group = v,
                Some(Field::Tags) => {
                    e.tags = v.split([';', '|', ',']).map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect()
                }
                Some(Field::Notes) => e.notes = value.trim().to_string(),
                Some(Field::Password) => e.password = value.to_string(),
                None => {}
            }
        }
        if e.device_id.len() != 9 {
            skipped.push(Skipped { line, reason: "err.id_digits" });
            continue;
        }
        if entries.iter().any(|(_, x): &(usize, EntryUi)| x.device_id == e.device_id) {
            skipped.push(Skipped { line, reason: "book_csv.repeated" });
            continue;
        }
        entries.push((line, e));
    }
    Ok((entries, skipped))
}

/// The book as CSV, with the header, without the passwords; a byte-order mark so
/// that Excel reads the accents.
pub fn export(entries: &[EntryUi]) -> String {
    let mut out = String::from("\u{feff}");
    out.push_str(&COLUMNS.join(","));
    out.push_str("\r\n");
    for e in entries {
        let fields = [
            e.device_id.clone(),
            e.name.clone(),
            e.company.clone(),
            e.contact.clone(),
            e.email.clone(),
            e.phone.clone(),
            e.group.clone(),
            e.tags.join("; "),
            e.notes.clone(),
        ];
        let quoted: Vec<String> = fields.iter().map(|f| quote(f)).collect();
        out.push_str(&quoted.join(","));
        out.push_str("\r\n");
    }
    out
}

fn quote(field: &str) -> String {
    // A leading = + - @ would be a formula in a spreadsheet: a quote keeps it text.
    let field = if field.starts_with(['=', '+', '-', '@']) { format!("'{field}") } else { field.to_string() };
    if field.contains([',', ';', '"', '\n', '\r']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field
    }
}

/// The records of the file with the line each starts on (RFC 4180: quoted fields
/// may hold separators, quotes doubled, line breaks).
fn records(text: &str, separator: char) -> Vec<(usize, Vec<String>)> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut line = 1;
    let mut start = 1;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted => {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    quoted = false;
                }
            }
            '"' if field.is_empty() => quoted = true,
            c if c == separator && !quoted => row.push(std::mem::take(&mut field)),
            '\r' if !quoted => {}
            '\n' if !quoted => {
                row.push(std::mem::take(&mut field));
                rows.push((start, std::mem::take(&mut row)));
                line += 1;
                start = line;
            }
            '\n' => {
                field.push('\n');
                line += 1;
            }
            c => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push((start, row));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_spreadsheet_with_semicolons() {
        let text = "\u{feff}Nome;ID;Cliente;Note;Etichette;Colonna in più\r\n\
                    PC reception;123 456 789;Rossi Srl;\"primo piano; vicino all'ingresso\";server; stampa;x\r\n\
                    ;12345;;;;\r\n\
                    Portatile;987654321;\"Bianchi \"\"&\"\" figli\";\"due\nrighe\";;\r\n\
                    Doppio;123456789;;;;\r\n";
        let (entries, skipped) = parse(text).unwrap();
        assert_eq!(entries.len(), 2);
        let (line, e) = &entries[0];
        assert_eq!(*line, 2);
        assert_eq!(e.device_id, "123456789");
        assert_eq!(e.name, "PC reception");
        assert_eq!(e.company, "Rossi Srl");
        assert_eq!(e.notes, "primo piano; vicino all'ingresso");
        // "server; stampa" was split by the separator: the tag column has "server".
        assert_eq!(e.tags, ["server"]);
        let (line, e) = &entries[1];
        assert_eq!(*line, 4);
        assert_eq!(e.company, "Bianchi \"&\" figli");
        assert_eq!(e.notes, "due\nrighe");
        assert_eq!(
            skipped,
            [Skipped { line: 3, reason: "err.id_digits" }, Skipped { line: 6, reason: "book_csv.repeated" }]
        );
    }

    #[test]
    fn export_and_back() {
        let e = EntryUi {
            device_id: "111222333".into(),
            name: "Server, rack 2".into(),
            company: "=SUM(A1)".into(),
            notes: "a \"quote\"".into(),
            group: "Clienti".into(),
            tags: vec!["linux".into(), "server".into()],
            password: "secret".into(),
            ..Default::default()
        };
        let csv = export(std::slice::from_ref(&e));
        assert!(!csv.contains("secret"), "no passwords in the file");
        let (entries, skipped) = parse(&csv).unwrap();
        assert!(skipped.is_empty());
        let back = &entries[0].1;
        assert_eq!(back.device_id, e.device_id);
        assert_eq!(back.name, e.name);
        assert_eq!(back.company, "'=SUM(A1)", "a formula stays text");
        assert_eq!(back.notes, e.notes);
        assert_eq!(back.group, e.group);
        assert_eq!(back.tags, e.tags);
    }

    #[test]
    fn needs_the_id_column() {
        assert_eq!(parse("name,company\nPC,Rossi\n").unwrap_err(), "book_csv.no_id_column");
        assert_eq!(parse("").unwrap_err(), "book_csv.empty");
    }
}
