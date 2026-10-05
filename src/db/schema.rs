// @generated automatically by Diesel CLI.

diesel::table! {
    users (id) {
        id -> Integer,
        username -> Text,
        email -> Nullable<Text>,
        password_hash -> Text,
        status -> Integer,
        level -> Integer,
        created_time -> Timestamp,
        last_login_date -> Nullable<Timestamp>,
        last_login_ip -> Nullable<Binary>,
        registration_ip -> Nullable<Binary>,
    }
}

diesel::table! {
    groups (id) {
        id -> Integer,
        name -> Text,
        tag -> Text,
        slug -> Text,
        description -> Nullable<Text>,
        created_time -> Timestamp,
        owner_id -> Integer,
    }
}

diesel::table! {
    group_members (group_id, user_id) {
        group_id -> Integer,
        user_id -> Integer,
        permissions -> Integer,
    }
}

diesel::table! {
    nyaa_main_categories (id) {
        id -> Integer,
        name -> Text,
    }
}

diesel::table! {
    nyaa_sub_categories (id, main_category_id) {
        id -> Integer,
        main_category_id -> Integer,
        name -> Text,
    }
}

diesel::table! {
    nyaa_torrents (id) {
        id -> Integer,
        info_hash -> Binary,
        display_name -> Text,
        torrent_name -> Text,
        information -> Text,
        description -> Text,
        filesize -> BigInt,
        encoding -> Text,
        flags -> Integer,
        uploader_id -> Nullable<Integer>,
        uploader_ip -> Nullable<Binary>,
        has_torrent -> Integer,
        comment_count -> Integer,
        created_time -> Timestamp,
        updated_time -> Timestamp,
        main_category_id -> Integer,
        sub_category_id -> Integer,
        group_id -> Nullable<Integer>,
    }
}

diesel::table! {
    nyaa_statistics (torrent_id) {
        torrent_id -> Integer,
        seed_count -> Integer,
        leech_count -> Integer,
        download_count -> Integer,
        last_updated -> Timestamp,
    }
}

diesel::table! {
    nyaa_comments (id) {
        id -> Integer,
        torrent_id -> Integer,
        user_id -> Nullable<Integer>,
        created_time -> Timestamp,
        edited_time -> Nullable<Timestamp>,
        text -> Text,
    }
}

diesel::table! {
    bans (id) {
        id -> Integer,
        created_time -> Timestamp,
        admin_id -> Integer,
        user_id -> Nullable<Integer>,
        user_ip -> Nullable<Binary>,
        reason -> Text,
    }
}

diesel::table! {
    user_preferences (user_id) {
        user_id -> Integer,
        hide_comments -> Integer,
    }
}

diesel::table! {
    user_sessions (id) {
        id -> Text,
        user_id -> Integer,
        created_time -> Timestamp,
        last_seen -> Timestamp,
        ip -> Nullable<Binary>,
    }
}

diesel::joinable!(groups -> users (owner_id));
diesel::joinable!(nyaa_torrents -> users (uploader_id));
diesel::joinable!(nyaa_torrents -> groups (group_id));
diesel::joinable!(nyaa_statistics -> nyaa_torrents (torrent_id));
diesel::joinable!(nyaa_comments -> nyaa_torrents (torrent_id));
diesel::joinable!(nyaa_comments -> users (user_id));
diesel::joinable!(user_preferences -> users (user_id));
diesel::joinable!(group_members -> groups (group_id));
diesel::joinable!(group_members -> users (user_id));
diesel::joinable!(user_sessions -> users (user_id));

diesel::allow_tables_to_appear_in_same_query!(
    users,
    groups,
    group_members,
    nyaa_main_categories,
    nyaa_sub_categories,
    nyaa_torrents,
    nyaa_statistics,
    nyaa_comments,
    bans,
    user_preferences,
    user_sessions,
);
