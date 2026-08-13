-- Company/site directory used to scope releases per company.
-- Seeded from the production `0_companies` export (oytqnijs_digisoft, MariaDB)
-- so the admin dashboard has the same sites to pick from. This table is a
-- directory living in our own Postgres DB — it is NOT the live digerp.com
-- MySQL database (no credentials/access to that from here), so edits made
-- here (add/update/delete site) do not touch production ERP data.
CREATE TABLE IF NOT EXISTS companies (
    id               BIGSERIAL   PRIMARY KEY,
    site_name        TEXT        NOT NULL,
    site_description TEXT,
    branch_          TEXT        NOT NULL,
    company_prefix   TEXT        NOT NULL DEFAULT '0_',
    company_url      TEXT        NOT NULL,
    site_key         TEXT        NOT NULL DEFAULT '',
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (company_url, company_prefix)
);

CREATE INDEX IF NOT EXISTS idx_companies_site_name ON companies (site_name);

INSERT INTO companies (id, site_name, site_description, branch_, company_prefix, company_url, site_key) VALUES
(1, 'Kirima', 'KIRIMA SLOPES DAIRY CO-OPERATIVE SOCIETY LTD', 'Kirima Slopes Dairy', '0_', 'https://kirima.digerp.com/', ''),
(2, 'Wakulima', 'MUKURWEINI WAKULIMA DAIRY LTD', 'Mukurweini Wakulima Dairy', '0_', 'https://wakulimadairy.digerp.com/', ''),
(3, 'Nyala', 'NYALA DAIRY MULTI-PURPOSE CO-OPERATIVE SOCIETY', 'Nyala Dairy', '0_', 'https://nyala.digerp.com/', ''),
(4, 'Spenza', 'spenza limited', 'Spenza Ltd', '0_', 'https://spenza.digerp.com/', ''),
(5, 'Meruhl', 'meru highlands dairy', 'Meru Higlands', '0_', 'https://meruhl.digerp.com/', ''),
(6, 'Meruhl', 'meru highlands dairy', 'Meru Supreme', '1_', 'https://meruhl.digerp.com/', ''),
(7, 'Meruhl', 'meru highlands dairy', 'Meru Woods', '2_', 'https://meruhl.digerp.com/', ''),
(8, 'Siche', 'siche dairy limited', 'siche dairy', '0_', 'https://sichedairy.digerp.com/', ''),
(9, 'Kiambaa', 'kiambaa dairy farmers co-operative society', 'Kiambaa Dairy', '0_', 'https://kiambaa.digerp.com/', ''),
(10, 'Farisi', 'farisi manufacturing industry limited', 'Farisi Manufacturing', '0_', 'https://farisi.digerp.com/', ''),
(11, 'Mama', 'Mama', 'Mama Millers', '0_', 'https://mamamillers.digerp.com/', ''),
(12, 'Marvel', 'Marvel Feeds Limited', 'Marvel Feeds', '0_', 'https://marvel.digerp.com/', ''),
(13, 'Sisibo', 'SISIBO WATER SPRINGS COMPANY LTD', 'Sisibo Springs', '0_', 'https://sisibo.digerp.com/', ''),
(14, 'Ikaiga', 'Ikaiga Community Based Organisation', 'Ikaiga Community', '0_', 'https://ikaiga.digerp.com/', ''),
(15, 'Budgetwear', 'Budgetwear Limited', 'Budgetwear Limited', '0_', 'https://budgetwear.digerp.com/', ''),
(16, 'Budgetwear', 'Fitzroy Traders', 'Fitzroy Traders', '1_', 'https://budgetwear.digerp.com/', ''),
(17, 'Jirani', 'Jirani Flour LTD', 'Jirani Flour LTD', '0_', 'https://jirani.digerp.com/', ''),
(18, 'Atlas', 'Atlas', 'Atlas', '0_', 'https://atlas.digerp.com/', ''),
(19, 'Nyaladev', 'DEV NYALA DAIRY  MULTI-PURPOSE CO-OPERATIVE SOCIETY', 'Nyala Dairy Dev', '0_', 'https://nyala.digerp.com/dev/', ''),
(20, 'Simplyfood', 'Simply Food Limited', 'Simply Food', '0_', 'https://simplyfood.digerp.com/', ''),
(21, 'Kirimadev', 'KIRIMA DEV  ', 'Kirima Dev  ', '0_', 'https://kirima.digerp.com/dev/', ''),
(22, 'Kwetu', 'Kwetu', 'Kwetu', '0_', 'https://kwetu.digerp.com/', ''),
(23, 'kt', 'kiambaa dev', 'Kiambaa Dairy dev', '0_', 'https://kiambaa.digerp.com/dev', ''),
(24, 'SISIBO DEV', 'SISIBO SPRINGS DEV', 'SISIBO DEV', '0_', 'https://sisibo.digerp.com/dev/', ''),
(25, 'Ikaigadev', 'Ikaiga Dev', 'Ikaiga Dev', '0_', 'https://ikaiga.digerp.com/dev/', ''),
(26, 'Marveldev', 'Marvel Dev Feeds Limited', 'Marvel Dev', '0_', 'https://marvel.digerp.com/dev/', ''),
(27, 'Kisumu Poultry', 'Kisumu Poultry', 'Kisumu Poultry', '0_', 'https://kisumupoultry.digerp.com/', ''),
(28, 'TITO', 'TITO dairy farmers co-operative society', 'TITO Dev', '0_', 'https://goofball-koala-pager.ngrok-free.dev/', ''),
(29, 'bw', 'Budgetwear dev', 'Budgetwear dev', '0_', 'https://budgetwear.digerp.com/dev/', ''),
(30, 'Siche Dev', 'Siche Dairy Limited Dev', 'Siche Dairy Dev ', '0_', 'https://sichedairy.digerp.com/dev/', ''),
(31, 'Simplyfooddev', 'Simply Food Limited Dev', 'Simply Food Dev', '0_', 'https://simplyfood.digerp.com/dev/', ''),
(32, 'Wakulimadev', 'MUKURWEINI WAKULIMA DAIRY DEV', 'Mukurweini Wakulima Dairy Dev', '0_', 'https://wakulimadairy.digerp.com/dev/', ''),
(33, 'Spenzadev', 'spenza limited Dev', 'Spenza Ltd Dev', '0_', 'https://spenza.digerp.com/dev/', ''),
(34, 'Demo', 'Digerp Demo', 'Digerp Demo', '0_', 'https://demo.digerp.com/', ''),
(35, 'Mamadev', 'Mamadev', 'Mama Millers Dev', '0_', 'https://mamamillers.digerp.com/mpya/', ''),
(37, 'bwdemo', 'DEMO WEAR LIMITED', 'DEMO WEAR LIMITED', '0_', 'https://budgetwear.digerp.com/demo/', ''),
(38, 'Jiranidev', 'Jirani Dev Flour LTD', 'Jirani Dev Flour LTD', '0_', 'https://jirani.digerp.com/dev/', ''),
(39, 'Stations', 'Stations', 'Stations', '0_', 'https://vitalacretail.digerp.com/', ''),
(40, 'Meruhldev', 'meru highlands dairy Dev', 'Meru Higlands Dev', '0_', 'https://meruhl.digerp.com/dev/', ''),
(41, 'Meruhldev', 'meru highlands dairy Dev', 'Meru Supreme Dev', '1_', 'https://meruhl.digerp.com/dev/', ''),
(42, 'Meruhldev', 'meru highlands dairy Dev', 'Meru Woods Dev', '2_', 'https://meruhl.digerp.com/dev/', ''),
(43, 'Atlasdev', 'Atlas Dev', 'Atlas Dev', '0_', 'https://atlas.digerp.com/dev/', ''),
(44, 'Falcon', 'Falcon', 'Falcon', '0_', 'https://falcon.digerp.com/', '')
ON CONFLICT (company_url, company_prefix) DO NOTHING;

-- Keep the sequence ahead of the highest seeded id (dump's AUTO_INCREMENT was 45)
SELECT setval('companies_id_seq', GREATEST((SELECT MAX(id) FROM companies), 44));
