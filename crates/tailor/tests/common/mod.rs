//! Invented people and companies, for the tests.
#![allow(dead_code)]

use nexus_tailor::{
    Contact, CvContent, CvEmployment, CvProject, CvSkills, Education, Employment, Facts, Language,
    Metric, RenderError, RenderRequest, Rendered, Renderer, Report, check,
};

pub fn facts_en() -> Facts {
    Facts {
        name: "Mara Example".to_owned(),
        contact: Contact {
            email: "mara@example.org".to_owned(),
            phone: "+49 170 5550137".to_owned(),
            linkedin: "linkedin.com/in/mara-example".to_owned(),
            github: "github.com/maraexample".to_owned(),
            location: "10115 Berlin".to_owned(),
        },
        employment: vec![
            Employment {
                id: "brightpath".to_owned(),
                company: "Brightpath Logistics".to_owned(),
                title: "Software Engineer".to_owned(),
                date: "03/2019 – 12/2022".to_owned(),
                location: "Hamburg".to_owned(),
            },
            Employment {
                id: "quillon".to_owned(),
                company: "Quillon Systems".to_owned(),
                title: "Backend Developer".to_owned(),
                date: "Jan 2016 – Feb 2019".to_owned(),
                location: "Bremen".to_owned(),
            },
            Employment {
                id: "tessel".to_owned(),
                company: "Tessel Works".to_owned(),
                title: "Working Student".to_owned(),
                date: "2014 – 2015".to_owned(),
                location: "Kiel".to_owned(),
            },
        ],
        education: vec![Education {
            id: "hochschule".to_owned(),
            institution: "Hochschule Nordkap".to_owned(),
            qualification: "Bachelor of Science, Computer Science".to_owned(),
            date: "2012 – 2015".to_owned(),
            location: "Kiel".to_owned(),
            details: "Thesis on route planning".to_owned(),
        }],
        languages: vec![
            Language {
                name: "German".to_owned(),
                level: "native".to_owned(),
            },
            Language {
                name: "English".to_owned(),
                level: "C1".to_owned(),
            },
        ],
        metrics: vec![Metric {
            key: "latency".to_owned(),
            text: "cut p95 latency by 40% across 1,200 services".to_owned(),
        }],
        allowed_tokens: vec!["24/7".to_owned()],
        banned_phrases: vec!["passionate team player".to_owned(), "Rockstar".to_owned()],
        vocabulary: vec![
            "AWS S3".to_owned(),
            "Kubernetes".to_owned(),
            "OAuth2".to_owned(),
            "Scrum Master".to_owned(),
        ],
    }
}

pub fn facts_de() -> Facts {
    Facts {
        name: "Jörg Müßig".to_owned(),
        contact: Contact {
            email: "joerg@example.org".to_owned(),
            phone: "+49 30 5550199".to_owned(),
            location: "Köln".to_owned(),
            ..Contact::default()
        },
        employment: vec![
            Employment {
                id: "rheinwerk".to_owned(),
                company: "Rheinwerk Maschinenbau GmbH".to_owned(),
                title: "Softwareentwickler".to_owned(),
                date: "März 2018 – Juni 2023".to_owned(),
                location: "Düsseldorf".to_owned(),
            },
            Employment {
                id: "lueftung".to_owned(),
                company: "Lüftungsbau Süd AG".to_owned(),
                title: "Technischer Redakteur".to_owned(),
                date: "2015 – 2018".to_owned(),
                location: "München".to_owned(),
            },
        ],
        education: vec![Education {
            id: "fh".to_owned(),
            institution: "Fachhochschule Bergisches Land".to_owned(),
            qualification: "Diplom-Informatiker (FH)".to_owned(),
            date: "2010 – 2015".to_owned(),
            location: "Wuppertal".to_owned(),
            details: String::new(),
        }],
        languages: vec![
            Language {
                name: "Deutsch".to_owned(),
                level: "Muttersprache".to_owned(),
            },
            Language {
                name: "Englisch".to_owned(),
                level: "verhandlungssicher".to_owned(),
            },
        ],
        metrics: vec![Metric {
            key: "kosten".to_owned(),
            text: "Wartungskosten um 15 % gesenkt".to_owned(),
        }],
        allowed_tokens: Vec::new(),
        banned_phrases: vec!["Teamplayer mit Leidenschaft".to_owned()],
        vocabulary: vec!["Kubernetes".to_owned(), "Großkunden".to_owned()],
    }
}

pub fn content_en() -> CvContent {
    CvContent {
        lang: "en".to_owned(),
        summary: "Backend engineer who keeps delivery systems boring, measurable and easy to \
                  change. I like small teams and plain tools."
            .to_owned(),
        skills: vec![
            CvSkills {
                category: "Languages".to_owned(),
                items: ["Rust", "Go", "Python", "SQL"].map(str::to_owned).to_vec(),
            },
            CvSkills {
                category: "Platform".to_owned(),
                items: ["Kubernetes", "AWS S3", "PostgreSQL", "Terraform"].map(str::to_owned).to_vec(),
            },
        ],
        employment: vec![
            CvEmployment {
                id: "brightpath".to_owned(),
                tier: 1,
                bullets: [
                    "Built the route-planning service that dispatches every delivery in northern Germany, running 24/7.",
                    "Replaced a nightly batch with an event stream, so drivers see changes within seconds.",
                    "Introduced property-based tests for the pricing rules and removed a whole class of billing errors.",
                    "Mentored two colleagues through their first on-call rotation.",
                ]
                .map(str::to_owned)
                .to_vec(),
            },
            CvEmployment {
                id: "quillon".to_owned(),
                tier: 2,
                bullets: [
                    "Moved authentication to OAuth2 without a maintenance window.",
                    "Wrote the runbooks the team still uses for database failover.",
                ]
                .map(str::to_owned)
                .to_vec(),
            },
            CvEmployment {
                id: "tessel".to_owned(),
                tier: 3,
                bullets: Vec::new(),
            },
        ],
        projects: vec![CvProject {
            name: "Pallet".to_owned(),
            url: "example.org/pallet".to_owned(),
            stack: "Rust, SQLite".to_owned(),
            description: "A small warehouse tracker for a local food bank.".to_owned(),
        }],
        education_ids: vec!["hochschule".to_owned()],
        languages: true,
    }
}

pub fn content_de() -> CvContent {
    CvContent {
        lang: "de".to_owned(),
        summary: "Softwareentwickler mit Blick für Dokumentation und Betrieb. Ich übersetze \
                  Anforderungen aus dem Maschinenbau in wartbare Systeme."
            .to_owned(),
        skills: vec![CvSkills {
            category: "Werkzeuge".to_owned(),
            items: ["Rust", "Python", "Kubernetes", "PostgreSQL"].map(str::to_owned).to_vec(),
        }],
        employment: vec![
            CvEmployment {
                id: "rheinwerk".to_owned(),
                tier: 1,
                bullets: [
                    "Steuerungssoftware für Großkunden entworfen und über mehrere Standorte hinweg ausgerollt.",
                    "Die Überwachung neu aufgebaut, sodass Störungen früher auffallen.",
                    "Übergabe an den Betrieb mit klaren Anleitungen abgesichert – auch für Nachtschichten.",
                ]
                .map(str::to_owned)
                .to_vec(),
            },
            CvEmployment {
                id: "lueftung".to_owned(),
                tier: 3,
                bullets: Vec::new(),
            },
        ],
        projects: Vec::new(),
        education_ids: vec!["fh".to_owned()],
        languages: true,
    }
}

pub async fn render(source: &str, facts: &Facts) -> Result<Rendered, RenderError> {
    Renderer::InProcess
        .render(RenderRequest {
            source: source.to_owned(),
            facts: facts.clone(),
            pdf: false,
            preview_pages: 0,
            stamp: false,
        })
        .await
}

pub async fn render_and_check(source: &str, facts: &Facts) -> Report {
    let rendered = render(source, facts)
        .await
        .unwrap_or_else(|e| panic!("does not compile: {e:?}\n{source}"));
    check(&rendered.text, facts, None, "")
}
